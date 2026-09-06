//! `coordinator → CLI`: [`ControlReply`] (blueprint §7.3, §24.1).
//!
//! Every [`crate::ControlRequest`] is answered by exactly one reply, on the
//! same connection, in order. There is no correlation id in the family because
//! the control leg is strictly request/response — the fan-out streams a request
//! *opens* (logs, topic taps) travel as [`crate::FrameKind::Log`] and
//! [`crate::FrameKind::Data`] frames carrying a [`crate::SubscriptionId`],
//! never as replies.
//!
//! # Beyond §24.1
//!
//! §24.1 froze eleven reply variants. Five more are appended at the tail,
//! which is the sanctioned way to extend a wire enum (§3, principle 4):
//!
//! | Index | Variant | Why |
//! |---:|---|---|
//! | 11 | [`ControlReply::Welcome`] | §7.2 requires a `Welcome`; §24.1's reply list names only its `Refused` sibling |
//! | 12 | [`ControlReply::BuildStarted`] | `Build` must return the [`crate::BuildId`] that `WaitForBuild` then names |
//! | 13 | [`ControlReply::Started`] | `Start` must return the [`crate::DataflowId`] the coordinator assigned |
//! | 14 | [`ControlReply::NodeMetrics`] | the answer to [`crate::ControlRequest::GetNodeMetrics`] (`astrs top`, §13), itself a tail append |
//! | 15 | [`ControlReply::Manifest`] | the answer to [`crate::ControlRequest::GetManifest`] (`astrs top`'s Graph tab, §5.2), itself a tail append |
//! | 16 | [`ControlReply::NodeIoMetrics`] | the answer to [`crate::ControlRequest::GetNodeIoMetrics`] (`astrs top`'s throughput column, §13), itself a tail append |
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{ControlReply, ErrorCode, WireMessage};
//!
//! let error = ControlReply::error(ErrorCode::NotFound, "no such dataflow");
//! assert!(error.is_error());
//! assert_eq!(error.error_code(), Some(ErrorCode::NotFound));
//! assert_eq!(error.variant_name(), "Error");
//! ```

use core::fmt;

use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::common::log::LogRecord;
use crate::common::metrics::{NodeIoSample, NodeMetricsSample};
use crate::common::status::{DaemonInfo, DataflowResult, DataflowSummary, NodeInfo};
use crate::common::stream::TraceData;
use crate::frame::FrameKind;
use crate::handshake::messages::{Refused, Welcome};
use crate::ids::{BuildId, DataflowId, ParamKey};
use crate::messages::control::types::{ErrorCode, ParamScope};
use crate::messages::impl_wire_message;
use crate::metadata::Parameter;

/// The coordinator → CLI message family (§24.1).
///
/// Variant indices 0–10 are the frozen §24.1 set; 11–13 are tail appends
/// documented in the module header.
///
/// # Examples
///
/// ```
/// use astrs_wire::{ControlReply, DataflowId};
///
/// let started = ControlReply::Started {
///     dataflow: DataflowId::from_u128(9),
///     name: Some("perception".to_owned()),
/// };
/// assert_eq!(started.dataflow(), Some(DataflowId::from_u128(9)));
/// assert!(started.is_success());
/// ```
// No `Eq`: `ParamValue` and `ParamList` carry [`Parameter`]s, which may hold an
// `f64`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ControlReply {
    /// The request succeeded and has nothing to report.
    #[oxicode(variant = 0)]
    Ok,
    /// The request failed.
    #[oxicode(variant = 1)]
    Error {
        /// The machine-readable classification.
        code: ErrorCode,
        /// The human-readable summary.
        message: String,
        /// The causal chain behind `message`, outermost first. Empty when the
        /// error had no deeper cause.
        context: Vec<String>,
    },
    /// The answer to `List` and `Info`.
    #[oxicode(variant = 2)]
    DataflowList {
        /// One row per dataflow.
        dataflows: Vec<DataflowSummary>,
        /// One row per node, when the request asked for node detail.
        nodes: Vec<NodeInfo>,
    },
    /// The outcome of a finished dataflow.
    #[oxicode(variant = 3)]
    DataflowResult {
        /// What happened.
        result: Box<DataflowResult>,
    },
    /// The answer to `GetNodeInfo`.
    #[oxicode(variant = 4)]
    NodeInfo {
        /// The nodes described; a single-element vector for `GetNodeInfo`.
        nodes: Vec<NodeInfo>,
    },
    /// The answer to `ConnectedDaemons`.
    #[oxicode(variant = 5)]
    DaemonList {
        /// One row per registered daemon.
        daemons: Vec<DaemonInfo>,
    },
    /// The answer to `Logs`.
    #[oxicode(variant = 6)]
    Logs {
        /// The records, oldest first.
        records: Vec<LogRecord>,
        /// Whether the query's limit cut the result short.
        truncated: bool,
    },
    /// The answer to `GetParam`.
    #[oxicode(variant = 7)]
    ParamValue {
        /// The key that was read.
        key: ParamKey,
        /// Its value, or `None` when the key is unset.
        value: Option<Parameter>,
        /// The scope the value actually came from, which may be a parent of
        /// the scope asked about when the lookup inherited.
        scope: ParamScope,
    },
    /// The answer to `GetParams`.
    #[oxicode(variant = 8)]
    ParamList {
        /// The scope that was listed.
        scope: ParamScope,
        /// The parameters, in key order.
        params: Vec<(ParamKey, Parameter)>,
    },
    /// The answer to `GetTraces`.
    #[oxicode(variant = 9)]
    TraceData {
        /// The collected spans.
        traces: TraceData,
    },
    /// The handshake was refused (§7.2).
    #[oxicode(variant = 10)]
    Refused(Refused),
    /// The handshake was accepted (§7.2) — a tail append beyond §24.1.
    #[oxicode(variant = 11)]
    Welcome(Welcome),
    /// A build was accepted and is now running — a tail append beyond §24.1.
    #[oxicode(variant = 12)]
    BuildStarted {
        /// The build to name in a later `WaitForBuild`.
        build: BuildId,
    },
    /// A dataflow was started — a tail append beyond §24.1.
    #[oxicode(variant = 13)]
    Started {
        /// The id the coordinator assigned.
        dataflow: DataflowId,
        /// The name it was started under, if any.
        name: Option<String>,
    },
    /// The answer to `GetNodeMetrics` (`astrs top`, §13) — a tail append
    /// beyond §24.1.
    #[oxicode(variant = 14)]
    NodeMetrics {
        /// The samples the coordinator currently holds for the requested
        /// dataflow/node — the coordinator's latest-known reading per node,
        /// not a history (blueprint §13: the daemon re-samples every two
        /// seconds, so a poll is never staler than that).
        samples: Vec<NodeMetricsSample>,
    },
    /// The answer to `GetManifest` (`astrs top`'s Graph tab) — a tail
    /// append beyond §24.1.
    #[oxicode(variant = 15)]
    Manifest {
        /// The dataflow's *expanded* manifest, re-serialized to YAML from
        /// the coordinator's registry — every `module:` reference already
        /// flattened (blueprint §8.5), so the receiving end can feed it
        /// straight to `Manifest::from_yaml_str` and
        /// `DataflowGraph::from_manifest` without a module loader or any
        /// filesystem access of its own.
        yaml: String,
        /// The directory the manifest's relative paths resolve against, for
        /// display only — a remote client cannot use it to actually resolve
        /// anything.
        working_dir: Option<String>,
    },
    /// The answer to [`crate::ControlRequest::GetNodeIoMetrics`] — a tail
    /// append beyond §24.1, and the CLI-side twin of
    /// [`crate::DaemonEvent::NodeIoMetrics`].
    #[oxicode(variant = 16)]
    NodeIoMetrics {
        /// The coordinator's latest bandwidth reading per node, under the
        /// same freshness rule as [`ControlReply::NodeMetrics`]: the daemon
        /// re-samples every two seconds, so a poll is never staler than that.
        samples: Vec<NodeIoSample>,
    },
}

impl ControlReply {
    /// An error reply with no causal chain.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{ControlReply, ErrorCode};
    ///
    /// let reply = ControlReply::error(ErrorCode::Timeout, "the daemon did not answer");
    /// assert_eq!(reply.error_code(), Some(ErrorCode::Timeout));
    /// ```
    #[must_use]
    pub fn error(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::Error {
            code,
            message: message.into(),
            context: Vec::new(),
        }
    }

    /// An error reply with a causal chain, outermost first.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{ControlReply, ErrorCode};
    ///
    /// let reply = ControlReply::error_with_context(
    ///     ErrorCode::BuildFailed,
    ///     "build failed",
    ///     ["cargo exited with code 101", "no such crate: camera-node"],
    /// );
    /// assert!(reply.to_string().contains("build failed"));
    /// ```
    #[must_use]
    pub fn error_with_context<I, S>(code: ErrorCode, message: impl Into<String>, context: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::Error {
            code,
            message: message.into(),
            context: context.into_iter().map(Into::into).collect(),
        }
    }

    /// Whether the request failed.
    #[must_use]
    pub const fn is_error(&self) -> bool {
        matches!(self, Self::Error { .. } | Self::Refused(_))
    }

    /// Whether the request succeeded.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        !self.is_error()
    }

    /// The error classification, if this reply is an error.
    ///
    /// A refusal maps onto [`ErrorCode::PermissionDenied`] or
    /// [`ErrorCode::Unsupported`] depending on why it was refused, so a client
    /// can treat every failure uniformly.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{ControlReply, ErrorCode, RefusalReason, Refused};
    ///
    /// let refused = ControlReply::Refused(Refused::new(RefusalReason::BadAuth));
    /// assert_eq!(refused.error_code(), Some(ErrorCode::PermissionDenied));
    /// assert_eq!(ControlReply::Ok.error_code(), None);
    /// ```
    #[must_use]
    pub const fn error_code(&self) -> Option<ErrorCode> {
        match self {
            Self::Error { code, .. } => Some(*code),
            Self::Refused(refused) => Some(match refused.reason {
                crate::handshake::messages::RefusalReason::BadAuth
                | crate::handshake::messages::RefusalReason::RoleNotPermitted { .. } => {
                    ErrorCode::PermissionDenied
                }
                crate::handshake::messages::RefusalReason::TooManyConnections { .. } => {
                    ErrorCode::ResourceExhausted
                }
                crate::handshake::messages::RefusalReason::ShuttingDown => ErrorCode::Unavailable,
                crate::handshake::messages::RefusalReason::UnknownSession { .. } => {
                    ErrorCode::NotFound
                }
                _ => ErrorCode::Unsupported,
            }),
            _ => None,
        }
    }

    /// The human-readable message, if this reply is an error.
    #[must_use]
    pub fn error_message(&self) -> Option<&str> {
        match self {
            Self::Error { message, .. } => Some(message),
            _ => None,
        }
    }

    /// The dataflow this reply is about, when it is about one.
    #[must_use]
    pub fn dataflow(&self) -> Option<DataflowId> {
        match self {
            Self::Started { dataflow, .. } => Some(*dataflow),
            Self::DataflowResult { result } => Some(result.dataflow),
            Self::DataflowList { dataflows, .. } if dataflows.len() == 1 => {
                dataflows.first().map(|summary| summary.id)
            }
            _ => None,
        }
    }

    /// Whether this reply completes a handshake, in either direction.
    #[must_use]
    pub const fn is_handshake(&self) -> bool {
        matches!(self, Self::Welcome(_) | Self::Refused(_))
    }

    /// The acceptance carried by a handshake reply, if it is one.
    #[must_use]
    pub const fn welcome(&self) -> Option<&Welcome> {
        match self {
            Self::Welcome(welcome) => Some(welcome),
            _ => None,
        }
    }

    /// The refusal carried by a handshake reply, if it is one.
    #[must_use]
    pub const fn refused(&self) -> Option<&Refused> {
        match self {
            Self::Refused(refused) => Some(refused),
            _ => None,
        }
    }

    /// How many rows this reply carries, for paging and log lines.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::ControlReply;
    ///
    /// assert_eq!(ControlReply::Ok.row_count(), 0);
    /// assert_eq!(
    ///     ControlReply::Logs { records: Vec::new(), truncated: false }.row_count(),
    ///     0
    /// );
    /// ```
    #[must_use]
    pub fn row_count(&self) -> usize {
        match self {
            Self::DataflowList { dataflows, nodes } => dataflows.len() + nodes.len(),
            Self::NodeInfo { nodes } => nodes.len(),
            Self::DaemonList { daemons } => daemons.len(),
            Self::Logs { records, .. } => records.len(),
            Self::ParamList { params, .. } => params.len(),
            Self::TraceData { traces } => traces.spans.len(),
            Self::NodeMetrics { samples } => samples.len(),
            _ => 0,
        }
    }

    /// Compares two replies with `f64` bit patterns rather than IEEE
    /// equality — see [`crate::PeerEvent::bitwise_eq`].
    #[must_use]
    pub fn bitwise_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::ParamValue {
                    key: left_key,
                    value: left_value,
                    scope: left_scope,
                },
                Self::ParamValue {
                    key: right_key,
                    value: right_value,
                    scope: right_scope,
                },
            ) => {
                left_key == right_key
                    && left_scope == right_scope
                    && match (left_value, right_value) {
                        (Some(left), Some(right)) => left.bitwise_eq(right),
                        (None, None) => true,
                        _ => false,
                    }
            }
            (
                Self::ParamList {
                    scope: left_scope,
                    params: left_params,
                },
                Self::ParamList {
                    scope: right_scope,
                    params: right_params,
                },
            ) => {
                left_scope == right_scope
                    && left_params.len() == right_params.len()
                    && left_params.iter().zip(right_params).all(
                        |((left_key, left_value), (right_key, right_value))| {
                            left_key == right_key && left_value.bitwise_eq(right_value)
                        },
                    )
            }
            _ => self == other,
        }
    }
}

impl fmt::Display for ControlReply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ok => f.write_str("ok"),
            Self::Error {
                code,
                message,
                context,
            } => {
                write!(f, "{code}: {message}")?;
                for cause in context {
                    write!(f, ": {cause}")?;
                }
                Ok(())
            }
            Self::DataflowList { dataflows, nodes } => write!(
                f,
                "{} dataflow(s), {} node row(s)",
                dataflows.len(),
                nodes.len()
            ),
            Self::DataflowResult { result } => {
                write!(f, "dataflow {} {}", result.dataflow, result.status)
            }
            Self::NodeInfo { nodes } => write!(f, "{} node(s)", nodes.len()),
            Self::DaemonList { daemons } => write!(f, "{} daemon(s)", daemons.len()),
            Self::Logs { records, truncated } => write!(
                f,
                "{} log record(s){}",
                records.len(),
                if *truncated { " (truncated)" } else { "" }
            ),
            Self::ParamValue { key, value, scope } => match value {
                Some(value) => write!(f, "{key} = {value} (from {scope})"),
                None => write!(f, "{key} is unset in {scope}"),
            },
            Self::ParamList { scope, params } => {
                write!(f, "{} parameter(s) in {scope}", params.len())
            }
            Self::TraceData { traces } => write!(f, "{} span(s)", traces.spans.len()),
            Self::Refused(refused) => write!(f, "refused: {refused}"),
            Self::Welcome(welcome) => write!(f, "{welcome}"),
            Self::BuildStarted { build } => write!(f, "build {build} started"),
            Self::Started { dataflow, name } => match name {
                Some(name) => write!(f, "dataflow {name} ({dataflow}) started"),
                None => write!(f, "dataflow {dataflow} started"),
            },
            Self::NodeMetrics { samples } => write!(f, "{} node metric sample(s)", samples.len()),
            Self::Manifest { yaml, .. } => write!(f, "manifest ({} bytes)", yaml.len()),
            Self::NodeIoMetrics { samples } => {
                write!(f, "{} bandwidth sample(s)", samples.len())
            }
        }
    }
}

impl_wire_message!(
    ControlReply,
    FrameKind::ControlReply,
    [
        "Ok",
        "Error",
        "DataflowList",
        "DataflowResult",
        "NodeInfo",
        "DaemonList",
        "Logs",
        "ParamValue",
        "ParamList",
        "TraceData",
        "Refused",
        "Welcome",
        "BuildStarted",
        "Started",
        "NodeMetrics",
        "Manifest",
        "NodeIoMetrics",
    ],
    fn variant_index(&self) -> u16 {
        match self {
            Self::Ok => 0,
            Self::Error { .. } => 1,
            Self::DataflowList { .. } => 2,
            Self::DataflowResult { .. } => 3,
            Self::NodeInfo { .. } => 4,
            Self::DaemonList { .. } => 5,
            Self::Logs { .. } => 6,
            Self::ParamValue { .. } => 7,
            Self::ParamList { .. } => 8,
            Self::TraceData { .. } => 9,
            Self::Refused(_) => 10,
            Self::Welcome(_) => 11,
            Self::BuildStarted { .. } => 12,
            Self::Started { .. } => 13,
            Self::NodeMetrics { .. } => 14,
            Self::Manifest { .. } => 15,
            Self::NodeIoMetrics { .. } => 16,
        }
    }
);

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_time::HlcTimestamp;

    use super::*;
    use crate::codec::{WireDecode, WireEncode, round_trip};
    use crate::common::status::DataflowStatus;
    use crate::frame::{FrameFlags, FrameLimits};
    use crate::handshake::messages::RefusalReason;
    use crate::messages::WireMessage;
    use crate::messages::samples::control_replies;

    #[test]
    fn the_family_freezes_the_eleven_normative_variants_first() {
        assert_eq!(
            &ControlReply::VARIANT_NAMES[..11],
            &[
                "Ok",
                "Error",
                "DataflowList",
                "DataflowResult",
                "NodeInfo",
                "DaemonList",
                "Logs",
                "ParamValue",
                "ParamList",
                "TraceData",
                "Refused",
            ]
        );
        assert_eq!(ControlReply::VARIANT_NAMES.len(), 17);
        assert_eq!(ControlReply::VARIANT_NAMES[14], "NodeMetrics");
        assert_eq!(ControlReply::VARIANT_NAMES[15], "Manifest");
        assert_eq!(ControlReply::VARIANT_NAMES[16], "NodeIoMetrics");
    }

    #[test]
    fn every_variant_reports_and_encodes_its_frozen_index() {
        let samples = control_replies().unwrap();
        assert_eq!(samples.len(), ControlReply::VARIANT_NAMES.len());
        for (index, sample) in samples.iter().enumerate() {
            let index = u16::try_from(index).unwrap();
            assert_eq!(sample.variant_index(), index, "{sample:?}");
            assert_eq!(u16::from(sample.encode_to_vec().unwrap()[0]), index);
            assert_eq!(
                sample.variant_name(),
                ControlReply::VARIANT_NAMES[usize::from(index)]
            );
        }
    }

    #[test]
    fn every_variant_round_trips_through_a_frame() {
        let limits = FrameLimits::network();
        for sample in control_replies().unwrap() {
            let bytes = sample.to_frame(FrameFlags::CRC, &limits).unwrap();
            let decoded = ControlReply::from_bytes(&bytes, &limits).unwrap();
            assert!(decoded.bitwise_eq(&sample), "{sample:?}");
        }
    }

    #[test]
    fn trailing_bytes_after_a_reply_are_refused() {
        for sample in control_replies().unwrap() {
            let mut bytes = sample.encode_to_vec().unwrap();
            bytes.push(1);
            assert!(matches!(
                ControlReply::decode_exact(&bytes),
                Err(crate::error::WireError::TrailingBytes { .. })
            ));
        }
    }

    #[test]
    fn success_and_failure_partition_the_family() {
        for sample in control_replies().unwrap() {
            assert_ne!(sample.is_error(), sample.is_success());
            assert_eq!(sample.is_error(), sample.error_code().is_some());
        }
    }

    #[test]
    fn errors_carry_their_code_message_and_chain() {
        let reply = ControlReply::error_with_context(
            ErrorCode::BuildFailed,
            "build failed",
            ["cargo exited with 101", "no such crate"],
        );
        assert_eq!(reply.error_code(), Some(ErrorCode::BuildFailed));
        assert_eq!(reply.error_message(), Some("build failed"));
        let text = reply.to_string();
        assert!(text.contains("build_failed"));
        assert!(text.contains("cargo exited with 101"));
        assert!(text.contains("no such crate"));

        let plain = ControlReply::error(ErrorCode::NotFound, "gone");
        match plain {
            ControlReply::Error { ref context, .. } => assert!(context.is_empty()),
            ref other => panic!("expected Error, got {other:?}"),
        }
        assert_eq!(round_trip(&plain).unwrap(), plain);
    }

    #[test]
    fn refusals_map_onto_error_codes() {
        let cases = [
            (RefusalReason::BadAuth, ErrorCode::PermissionDenied),
            (
                RefusalReason::RoleNotPermitted {
                    role: crate::handshake::role::Role::Node,
                },
                ErrorCode::PermissionDenied,
            ),
            (
                RefusalReason::TooManyConnections { limit: 2 },
                ErrorCode::ResourceExhausted,
            ),
            (RefusalReason::ShuttingDown, ErrorCode::Unavailable),
            (
                RefusalReason::UnknownSession {
                    session: crate::ids::SessionId::from_u128(1),
                },
                ErrorCode::NotFound,
            ),
            (
                RefusalReason::ProtocolTooOld {
                    peer: 0,
                    minimum: 1,
                },
                ErrorCode::Unsupported,
            ),
        ];
        for (reason, expected) in cases {
            let reply = ControlReply::Refused(Refused::new(reason));
            assert_eq!(reply.error_code(), Some(expected));
            assert!(reply.is_error());
            assert!(reply.is_handshake());
            assert!(reply.refused().is_some());
            assert!(reply.welcome().is_none());
        }
    }

    #[test]
    fn row_counts_reflect_the_payload() {
        let summary = DataflowSummary::pending(DataflowId::from_u128(1), 3);
        let reply = ControlReply::DataflowList {
            dataflows: vec![summary.clone(), summary],
            nodes: Vec::new(),
        };
        assert_eq!(reply.row_count(), 2);
        assert_eq!(ControlReply::Ok.row_count(), 0);
        assert_eq!(
            ControlReply::Logs {
                records: vec![LogRecord::new(
                    HlcTimestamp::new(1, 0),
                    crate::common::LogLevel::Info,
                    "x"
                )],
                truncated: true,
            }
            .row_count(),
            1
        );
    }

    #[test]
    fn dataflow_addressing_is_reported_where_it_exists() {
        let id = DataflowId::from_u128(0x99);
        assert_eq!(
            ControlReply::Started {
                dataflow: id,
                name: None
            }
            .dataflow(),
            Some(id)
        );

        let mut result = DataflowResult::new(id, HlcTimestamp::new(1, 0));
        result.status = DataflowStatus::Finished;
        assert_eq!(
            ControlReply::DataflowResult {
                result: Box::new(result)
            }
            .dataflow(),
            Some(id)
        );

        assert_eq!(
            ControlReply::DataflowList {
                dataflows: vec![DataflowSummary::pending(id, 1)],
                nodes: Vec::new(),
            }
            .dataflow(),
            Some(id)
        );
        assert_eq!(ControlReply::Ok.dataflow(), None);
    }

    #[test]
    fn node_metrics_reply_carries_its_samples() {
        use astrs_time::HlcTimestamp;

        let sample = crate::common::NodeMetricsSample::new(
            crate::ids::NodeId::new("camera").unwrap(),
            HlcTimestamp::new(1, 0),
        );
        let reply = ControlReply::NodeMetrics {
            samples: vec![sample],
        };
        assert_eq!(reply.row_count(), 1);
        assert!(reply.is_success());
        assert!(reply.to_string().contains("1 node metric"));
        assert_eq!(round_trip(&reply).unwrap(), reply);
    }

    #[test]
    fn manifest_reply_round_trips_its_yaml() {
        let reply = ControlReply::Manifest {
            yaml: "nodes:\n  - id: camera\n    path: ./camera\n".to_owned(),
            working_dir: Some("/workspace/demo".to_owned()),
        };
        assert!(reply.is_success());
        assert!(reply.to_string().contains("bytes"));
        assert_eq!(round_trip(&reply).unwrap(), reply);
    }

    #[test]
    fn nan_parameters_survive_the_wire() {
        let reply = ControlReply::ParamValue {
            key: ParamKey::new("gain").unwrap(),
            value: Some(Parameter::Float(f64::NAN)),
            scope: ParamScope::Global,
        };
        let decoded = round_trip(&reply).unwrap();
        assert_ne!(decoded, reply);
        assert!(decoded.bitwise_eq(&reply));

        let list = ControlReply::ParamList {
            scope: ParamScope::Global,
            params: vec![(ParamKey::new("gain").unwrap(), Parameter::Float(f64::NAN))],
        };
        assert!(round_trip(&list).unwrap().bitwise_eq(&list));
    }

    #[test]
    fn display_names_every_variant_without_panicking() {
        for sample in control_replies().unwrap() {
            assert!(
                !sample.to_string().is_empty(),
                "{} rendered empty",
                sample.variant_name()
            );
        }
    }

    #[test]
    fn serde_round_trips_the_family_for_diagnostics() {
        for sample in control_replies().unwrap() {
            let json = serde_json::to_string(&sample).unwrap();
            let back: ControlReply = serde_json::from_str(&json).unwrap();
            assert!(back.bitwise_eq(&sample));
        }
    }
}
