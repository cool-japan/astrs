//! The vocabulary the CLI ↔ coordinator family needs and nobody else does:
//! request scopes, dataflow sources, query filters, parameter scopes and error
//! codes.
//!
//! Everything here is shaped by one of two blueprint requirements:
//!
//! - **§16, capability posture.** *"the coordinator API distinguishes read
//!   verbs (list/logs/topic) from mutating verbs (start/stop/param) — token
//!   scopes are 0.2; the enum split lands now so it's not a breaking
//!   change later."* That is [`RequestScope`], which every
//!   [`crate::ControlRequest`] answers for.
//! - **§17, the CLI verb set.** Filters like [`LogQuery`] and [`TopicQuery`]
//!   exist so `astrs logs --since` and `astrs topic echo --hz` are served by
//!   the coordinator rather than by a client that downloads everything and
//!   throws most of it away.

use core::fmt;

use astrs_time::HlcTimestamp;
use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::common::log::LogLevel;
use crate::ids::{BuildId, DataflowId, NodeId};

/// Whether a request only observes the cluster or changes it (§16).
///
/// Token scopes arrive in 0.2; the classification lands now so that adding
/// them is not a breaking change. Every [`crate::ControlRequest`] answers
/// [`crate::ControlRequest::scope`], and an exhaustive `match` there means a
/// verb added later cannot compile until someone has decided which side of the
/// line it falls on.
///
/// # Examples
///
/// ```
/// use astrs_wire::{ControlRequest, RequestScope};
///
/// assert_eq!(ControlRequest::List { all: false }.scope(), RequestScope::Read);
/// assert!(ControlRequest::Destroy { force: false }.is_mutating());
/// ```
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    Encode,
    Decode,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RequestScope {
    /// The request observes; it changes nothing an operator could notice.
    #[default]
    #[oxicode(variant = 0)]
    Read,
    /// The request changes cluster state: it starts, stops, edits or destroys.
    #[oxicode(variant = 1)]
    Mutate,
}

impl RequestScope {
    /// Both scopes, in wire-index order.
    pub const ALL: &'static [Self] = &[Self::Read, Self::Mutate];

    /// A stable, lower-case name for logs, metrics labels and audit records.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Mutate => "mutate",
        }
    }

    /// Whether this scope changes cluster state.
    #[must_use]
    pub const fn is_mutating(self) -> bool {
        matches!(self, Self::Mutate)
    }
}

impl fmt::Display for RequestScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a dataflow to start comes from.
///
/// `astrs start` accepts either a manifest to expand on the spot or the id of
/// a build the coordinator already prepared (§17). Modelling that as one enum
/// rather than two optional fields makes the impossible states — both set,
/// neither set — unrepresentable.
///
/// # Examples
///
/// ```
/// use astrs_wire::{BuildId, DataflowSource};
///
/// let inline = DataflowSource::from_manifest("nodes: []");
/// assert!(inline.manifest_text().is_some());
///
/// let built = DataflowSource::Build { build: BuildId::from_u128(1) };
/// assert!(built.manifest_text().is_none());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DataflowSource {
    /// The manifest text itself, to be parsed and expanded by the coordinator.
    #[oxicode(variant = 0)]
    Manifest {
        /// The YAML source (§8).
        yaml: String,
        /// The directory relative paths inside the manifest resolve against.
        working_dir: Option<String>,
    },
    /// A build the coordinator already produced.
    #[oxicode(variant = 1)]
    Build {
        /// The build to start.
        build: BuildId,
    },
}

impl DataflowSource {
    /// An inline manifest with no working directory.
    #[must_use]
    pub fn from_manifest(yaml: impl Into<String>) -> Self {
        Self::Manifest {
            yaml: yaml.into(),
            working_dir: None,
        }
    }

    /// The manifest text, if this source carries one.
    #[must_use]
    pub fn manifest_text(&self) -> Option<&str> {
        match self {
            Self::Manifest { yaml, .. } => Some(yaml),
            Self::Build { .. } => None,
        }
    }

    /// The build id, if this source names one.
    #[must_use]
    pub const fn build(&self) -> Option<BuildId> {
        match self {
            Self::Manifest { .. } => None,
            Self::Build { build } => Some(*build),
        }
    }
}

impl fmt::Display for DataflowSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Manifest { yaml, working_dir } => match working_dir {
                Some(dir) => write!(f, "manifest ({} bytes) in {dir}", yaml.len()),
                None => write!(f, "manifest ({} bytes)", yaml.len()),
            },
            Self::Build { build } => write!(f, "build {build}"),
        }
    }
}

/// The default number of log records a single fetch returns.
pub const DEFAULT_LOG_LIMIT: u32 = 1_000;

/// A filter over the log stream, evaluated by the coordinator (§17 `logs`).
///
/// Filtering at the source is not a nicety: a busy dataflow produces far more
/// log volume than a `-f` tail is meant to carry, and a client-side filter
/// would spend the bandwidth anyway.
///
/// # Examples
///
/// ```
/// use astrs_wire::{LogLevel, LogQuery};
///
/// let query = LogQuery::default()
///     .with_min_level(LogLevel::Warn)
///     .with_contains("queue");
/// assert_eq!(query.min_level, Some(LogLevel::Warn));
/// assert_eq!(query.limit, Some(1_000));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct LogQuery {
    /// Drop records below this level.
    pub min_level: Option<LogLevel>,
    /// Drop records stamped before this instant.
    pub since: Option<HlcTimestamp>,
    /// Drop records stamped at or after this instant.
    pub until: Option<HlcTimestamp>,
    /// Keep only records whose message contains this substring.
    pub contains: Option<String>,
    /// Keep only records from this logging target (`astrs_daemon::spawn`).
    pub target: Option<String>,
    /// Stop after this many records. `None` means "no cap", which a
    /// coordinator may still bound by its own policy.
    pub limit: Option<u32>,
}

impl LogQuery {
    /// An unfiltered query with the default record cap.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            min_level: None,
            since: None,
            until: None,
            contains: None,
            target: None,
            limit: Some(DEFAULT_LOG_LIMIT),
        }
    }

    /// Filters out records below `level`.
    #[must_use]
    pub const fn with_min_level(mut self, level: LogLevel) -> Self {
        self.min_level = Some(level);
        self
    }

    /// Restricts the query to a time window.
    #[must_use]
    pub const fn with_window(mut self, since: HlcTimestamp, until: HlcTimestamp) -> Self {
        self.since = Some(since);
        self.until = Some(until);
        self
    }

    /// Keeps only records whose message contains `needle`.
    #[must_use]
    pub fn with_contains(mut self, needle: impl Into<String>) -> Self {
        self.contains = Some(needle.into());
        self
    }

    /// Keeps only records from `target`.
    #[must_use]
    pub fn with_target(mut self, target: impl Into<String>) -> Self {
        self.target = Some(target.into());
        self
    }

    /// Caps the number of records returned.
    #[must_use]
    pub const fn with_limit(mut self, limit: Option<u32>) -> Self {
        self.limit = limit;
        self
    }

    /// Whether this query filters anything at all.
    #[must_use]
    pub const fn is_unfiltered(&self) -> bool {
        self.min_level.is_none()
            && self.since.is_none()
            && self.until.is_none()
            && self.contains.is_none()
            && self.target.is_none()
    }

    /// Whether `record` passes this filter.
    ///
    /// Implemented here, in the crate both ends share, so a coordinator's
    /// server-side filter and a client's local re-filter can never disagree
    /// about what `--since` means.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_time::HlcTimestamp;
    /// use astrs_wire::{LogLevel, LogQuery, LogRecord};
    ///
    /// let record = LogRecord::new(HlcTimestamp::new(10, 0), LogLevel::Info, "queue full");
    /// assert!(LogQuery::new().matches(&record));
    /// assert!(!LogQuery::new().with_min_level(LogLevel::Error).matches(&record));
    /// assert!(LogQuery::new().with_contains("queue").matches(&record));
    /// ```
    #[must_use]
    pub fn matches(&self, record: &crate::common::LogRecord) -> bool {
        if let Some(level) = self.min_level
            && !record.level.is_enabled_at(level)
        {
            return false;
        }
        if let Some(since) = self.since
            && record.timestamp < since
        {
            return false;
        }
        if let Some(until) = self.until
            && record.timestamp >= until
        {
            return false;
        }
        if let Some(needle) = &self.contains
            && !record.message.contains(needle.as_str())
        {
            return false;
        }
        if let Some(target) = &self.target
            && record.target != *target
        {
            return false;
        }
        true
    }
}

impl Default for LogQuery {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for LogQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_unfiltered() {
            return f.write_str("all records");
        }
        let mut parts: Vec<String> = Vec::new();
        if let Some(level) = self.min_level {
            parts.push(format!("level ≥ {level}"));
        }
        if let Some(since) = self.since {
            parts.push(format!("since {since}"));
        }
        if let Some(until) = self.until {
            parts.push(format!("until {until}"));
        }
        if let Some(needle) = &self.contains {
            parts.push(format!("containing {needle:?}"));
        }
        if let Some(target) = &self.target {
            parts.push(format!("target {target}"));
        }
        f.write_str(&parts.join(", "))
    }
}

/// How a topic tap should be shaped (§17 `topic echo/hz`).
///
/// A camera topic at 30 Hz with 4 MiB frames is 120 MB/s; a terminal wants
/// perhaps one frame a second and only its header. These knobs let the CLI say
/// so, and the daemon drop the rest before it ever reaches the wire.
///
/// # Examples
///
/// ```
/// use astrs_wire::TopicQuery;
///
/// let query = TopicQuery::default().with_max_rate_hz(Some(1)).with_head_bytes(Some(64));
/// assert_eq!(query.max_rate_hz, Some(1));
/// assert_eq!(query.min_interval_ms(), Some(1_000));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct TopicQuery {
    /// Deliver at most this many messages per second.
    pub max_rate_hz: Option<u32>,
    /// Deliver only the first this-many payload bytes of each message.
    pub head_bytes: Option<u32>,
    /// Include the [`crate::Metadata`] beside each payload.
    pub include_metadata: bool,
    /// Deliver only the newest message when the consumer falls behind, rather
    /// than queueing (the `queue_size: 1` posture of §6.4).
    pub latest_only: bool,
    /// Stop after this many messages, then end the subscription.
    pub limit: Option<u64>,
}

impl TopicQuery {
    /// An unshaped tap: everything, with metadata.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_rate_hz: None,
            head_bytes: None,
            include_metadata: true,
            latest_only: false,
            limit: None,
        }
    }

    /// Caps the delivery rate.
    #[must_use]
    pub const fn with_max_rate_hz(mut self, rate: Option<u32>) -> Self {
        self.max_rate_hz = rate;
        self
    }

    /// Truncates each payload to its first bytes.
    #[must_use]
    pub const fn with_head_bytes(mut self, bytes: Option<u32>) -> Self {
        self.head_bytes = bytes;
        self
    }

    /// Includes or omits metadata.
    #[must_use]
    pub const fn with_metadata(mut self, include: bool) -> Self {
        self.include_metadata = include;
        self
    }

    /// Turns the tap into a latest-only one.
    #[must_use]
    pub const fn with_latest_only(mut self, latest_only: bool) -> Self {
        self.latest_only = latest_only;
        self
    }

    /// Ends the subscription after `limit` messages.
    #[must_use]
    pub const fn with_limit(mut self, limit: Option<u64>) -> Self {
        self.limit = limit;
        self
    }

    /// The minimum gap between deliveries implied by the rate cap, in
    /// milliseconds.
    ///
    /// Returns `None` when there is no cap; a cap of zero is read as "no
    /// deliveries", giving [`DurationMs::MAX`](crate::DurationMs::MAX)'s
    /// millisecond value rather than a division by zero.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::TopicQuery;
    ///
    /// assert_eq!(TopicQuery::new().min_interval_ms(), None);
    /// assert_eq!(TopicQuery::new().with_max_rate_hz(Some(4)).min_interval_ms(), Some(250));
    /// assert_eq!(TopicQuery::new().with_max_rate_hz(Some(0)).min_interval_ms(), Some(u64::MAX));
    /// ```
    #[must_use]
    pub const fn min_interval_ms(&self) -> Option<u64> {
        match self.max_rate_hz {
            None => None,
            Some(0) => Some(u64::MAX),
            Some(rate) => Some(1_000 / rate as u64),
        }
    }

    /// Whether the tap delivers payload bytes at all.
    #[must_use]
    pub const fn wants_payload(&self) -> bool {
        !matches!(self.head_bytes, Some(0))
    }
}

impl Default for TopicQuery {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for TopicQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts: Vec<String> = Vec::new();
        if let Some(rate) = self.max_rate_hz {
            parts.push(format!("≤{rate} Hz"));
        }
        if let Some(bytes) = self.head_bytes {
            parts.push(format!("first {bytes} B"));
        }
        if self.latest_only {
            parts.push("latest only".to_owned());
        }
        if let Some(limit) = self.limit {
            parts.push(format!("{limit} messages"));
        }
        if !self.include_metadata {
            parts.push("no metadata".to_owned());
        }
        if parts.is_empty() {
            return f.write_str("everything");
        }
        f.write_str(&parts.join(", "))
    }
}

/// Which parameter namespace a request addresses (§17 `param`).
///
/// Parameters nest: a node reads its own value, falling back to its dataflow's
/// and then the cluster's. The scope names the level a request means, so
/// `astrs param set --node camera exposure 12` cannot accidentally rewrite the
/// cluster default.
///
/// # Examples
///
/// ```
/// use astrs_wire::{DataflowId, NodeId, ParamScope};
///
/// let node = ParamScope::node(DataflowId::from_u128(1), NodeId::new("camera")?);
/// assert_eq!(node.dataflow(), Some(DataflowId::from_u128(1)));
/// assert!(node.parent().is_some());
/// assert_eq!(ParamScope::Global.parent(), None);
/// # Ok::<(), astrs_wire::IdError>(())
/// ```
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Encode, Decode,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ParamScope {
    /// Cluster-wide defaults, held by the coordinator.
    #[oxicode(variant = 0)]
    Global,
    /// One dataflow's parameters.
    #[oxicode(variant = 1)]
    Dataflow {
        /// The dataflow.
        dataflow: DataflowId,
    },
    /// One node's parameters.
    #[oxicode(variant = 2)]
    Node {
        /// The dataflow the node belongs to.
        dataflow: DataflowId,
        /// The node.
        node: NodeId,
    },
}

impl ParamScope {
    /// The scope of one node's parameters.
    #[must_use]
    pub const fn node(dataflow: DataflowId, node: NodeId) -> Self {
        Self::Node { dataflow, node }
    }

    /// The scope of one dataflow's parameters.
    #[must_use]
    pub const fn dataflow_scope(dataflow: DataflowId) -> Self {
        Self::Dataflow { dataflow }
    }

    /// The dataflow this scope belongs to, if it is not global.
    #[must_use]
    pub const fn dataflow(&self) -> Option<DataflowId> {
        match self {
            Self::Global => None,
            Self::Dataflow { dataflow } | Self::Node { dataflow, .. } => Some(*dataflow),
        }
    }

    /// The node this scope names, if it names one.
    #[must_use]
    pub const fn node_id(&self) -> Option<&NodeId> {
        match self {
            Self::Node { node, .. } => Some(node),
            _ => None,
        }
    }

    /// The scope a lookup falls back to when this one has no value.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{DataflowId, NodeId, ParamScope};
    ///
    /// let node = ParamScope::node(DataflowId::from_u128(1), NodeId::new("camera")?);
    /// assert_eq!(node.parent(), Some(ParamScope::dataflow_scope(DataflowId::from_u128(1))));
    /// assert_eq!(node.parent().and_then(|scope| scope.parent()), Some(ParamScope::Global));
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        match self {
            Self::Global => None,
            Self::Dataflow { .. } => Some(Self::Global),
            Self::Node { dataflow, .. } => Some(Self::Dataflow {
                dataflow: *dataflow,
            }),
        }
    }

    /// This scope and every scope it falls back to, nearest first.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{DataflowId, NodeId, ParamScope};
    ///
    /// let node = ParamScope::node(DataflowId::from_u128(1), NodeId::new("camera")?);
    /// assert_eq!(node.lookup_chain().len(), 3);
    /// assert_eq!(ParamScope::Global.lookup_chain().len(), 1);
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    #[must_use]
    pub fn lookup_chain(&self) -> Vec<Self> {
        let mut chain = vec![self.clone()];
        let mut current = self.clone();
        while let Some(parent) = current.parent() {
            chain.push(parent.clone());
            current = parent;
        }
        chain
    }

    /// A stable, lower-case name for logs and metrics labels.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Dataflow { .. } => "dataflow",
            Self::Node { .. } => "node",
        }
    }
}

impl fmt::Display for ParamScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Global => f.write_str("global"),
            Self::Dataflow { dataflow } => write!(f, "dataflow {dataflow}"),
            Self::Node { dataflow, node } => write!(f, "node {node} of dataflow {dataflow}"),
        }
    }
}

/// The machine-readable half of a [`crate::ControlReply::Error`].
///
/// The CLI turns these into exit codes and the TUI into colours, so the *code*
/// is the contract and the message is for humans. Codes are deliberately close
/// to the vocabulary every operator already knows from HTTP and gRPC.
///
/// # Examples
///
/// ```
/// use astrs_wire::ErrorCode;
///
/// assert!(ErrorCode::Unavailable.is_retryable());
/// assert!(!ErrorCode::NotFound.is_retryable());
/// assert_eq!(ErrorCode::NotFound.exit_code(), 4);
/// ```
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    Encode,
    Decode,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorCode {
    /// Something went wrong that has no better code.
    #[default]
    #[oxicode(variant = 0)]
    Internal,
    /// The request named something that does not exist.
    #[oxicode(variant = 1)]
    NotFound,
    /// The request would create something that already exists.
    #[oxicode(variant = 2)]
    AlreadyExists,
    /// The request is malformed or self-contradictory.
    #[oxicode(variant = 3)]
    InvalidArgument,
    /// The cluster is in a state where this request makes no sense — stopping
    /// a dataflow that never started, for instance.
    #[oxicode(variant = 4)]
    FailedPrecondition,
    /// The caller is not allowed to do this (§16).
    #[oxicode(variant = 5)]
    PermissionDenied,
    /// A required participant is not reachable right now.
    #[oxicode(variant = 6)]
    Unavailable,
    /// The request took longer than its deadline.
    #[oxicode(variant = 7)]
    Timeout,
    /// A resource ceiling was hit.
    #[oxicode(variant = 8)]
    ResourceExhausted,
    /// This build does not implement the request.
    #[oxicode(variant = 9)]
    Unsupported,
    /// The request was cancelled before it completed.
    #[oxicode(variant = 10)]
    Cancelled,
    /// The manifest or graph failed validation (§8, §5.2 `astrs-graph`).
    #[oxicode(variant = 11)]
    ValidationFailed,
    /// A build step failed.
    #[oxicode(variant = 12)]
    BuildFailed,
}

impl ErrorCode {
    /// Every code, in wire-index order.
    pub const ALL: &'static [Self] = &[
        Self::Internal,
        Self::NotFound,
        Self::AlreadyExists,
        Self::InvalidArgument,
        Self::FailedPrecondition,
        Self::PermissionDenied,
        Self::Unavailable,
        Self::Timeout,
        Self::ResourceExhausted,
        Self::Unsupported,
        Self::Cancelled,
        Self::ValidationFailed,
        Self::BuildFailed,
    ];

    /// A stable, lower-case name for logs, metrics labels and `--json` output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Internal => "internal",
            Self::NotFound => "not_found",
            Self::AlreadyExists => "already_exists",
            Self::InvalidArgument => "invalid_argument",
            Self::FailedPrecondition => "failed_precondition",
            Self::PermissionDenied => "permission_denied",
            Self::Unavailable => "unavailable",
            Self::Timeout => "timeout",
            Self::ResourceExhausted => "resource_exhausted",
            Self::Unsupported => "unsupported",
            Self::Cancelled => "cancelled",
            Self::ValidationFailed => "validation_failed",
            Self::BuildFailed => "build_failed",
        }
    }

    /// Whether repeating the identical request could plausibly succeed.
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::Unavailable | Self::Timeout | Self::ResourceExhausted | Self::Internal
        )
    }

    /// Whether the caller's request was at fault, as opposed to the cluster.
    #[must_use]
    pub const fn is_caller_error(self) -> bool {
        matches!(
            self,
            Self::NotFound
                | Self::AlreadyExists
                | Self::InvalidArgument
                | Self::FailedPrecondition
                | Self::PermissionDenied
                | Self::Unsupported
                | Self::ValidationFailed
        )
    }

    /// The process exit code the CLI uses for this error.
    ///
    /// Distinct small integers, chosen so a shell script can branch on the
    /// class of failure without parsing text. `1` stays the generic failure.
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Internal => 1,
            Self::Cancelled => 2,
            Self::InvalidArgument => 3,
            Self::NotFound => 4,
            Self::AlreadyExists => 5,
            Self::FailedPrecondition => 6,
            Self::PermissionDenied => 7,
            Self::Unavailable => 8,
            Self::Timeout => 9,
            Self::ResourceExhausted => 10,
            Self::Unsupported => 11,
            Self::ValidationFailed => 12,
            Self::BuildFailed => 13,
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::codec::{WireEncode, round_trip};
    use crate::common::LogRecord;

    #[test]
    fn request_scope_indices_are_frozen() {
        for (index, scope) in RequestScope::ALL.iter().enumerate() {
            assert_eq!(usize::from(scope.encode_to_vec().unwrap()[0]), index);
        }
        assert!(RequestScope::Mutate.is_mutating());
        assert!(!RequestScope::Read.is_mutating());
        assert_eq!(RequestScope::default(), RequestScope::Read);
        assert_eq!(RequestScope::Mutate.to_string(), "mutate");
    }

    #[test]
    fn dataflow_sources_are_exclusive_by_construction() {
        let inline = DataflowSource::from_manifest("nodes: []");
        assert_eq!(inline.manifest_text(), Some("nodes: []"));
        assert_eq!(inline.build(), None);
        assert_eq!(round_trip(&inline).unwrap(), inline);

        let built = DataflowSource::Build {
            build: BuildId::from_u128(3),
        };
        assert_eq!(built.manifest_text(), None);
        assert_eq!(built.build(), Some(BuildId::from_u128(3)));
        assert_eq!(usize::from(built.encode_to_vec().unwrap()[0]), 1);
        assert!(built.to_string().contains("build"));
        assert!(inline.to_string().contains("manifest"));
    }

    #[test]
    fn a_default_log_query_is_capped_but_unfiltered() {
        let query = LogQuery::default();
        assert!(query.is_unfiltered());
        assert_eq!(query.limit, Some(DEFAULT_LOG_LIMIT));
        assert_eq!(query.to_string(), "all records");
    }

    #[test]
    fn log_queries_filter_exactly_what_they_name() {
        let record = LogRecord::new(HlcTimestamp::new(100, 0), LogLevel::Info, "queue full")
            .with_target("astrs_daemon::spawn");

        assert!(LogQuery::new().matches(&record));
        assert!(
            LogQuery::new()
                .with_min_level(LogLevel::Info)
                .matches(&record)
        );
        assert!(
            !LogQuery::new()
                .with_min_level(LogLevel::Error)
                .matches(&record)
        );
        assert!(LogQuery::new().with_contains("queue").matches(&record));
        assert!(!LogQuery::new().with_contains("camera").matches(&record));
        assert!(
            LogQuery::new()
                .with_target("astrs_daemon::spawn")
                .matches(&record)
        );
        assert!(!LogQuery::new().with_target("other").matches(&record));
        assert!(
            LogQuery::new()
                .with_window(HlcTimestamp::new(0, 0), HlcTimestamp::new(200, 0))
                .matches(&record)
        );
        assert!(
            !LogQuery::new()
                .with_window(HlcTimestamp::new(200, 0), HlcTimestamp::new(300, 0))
                .matches(&record)
        );
        assert!(
            !LogQuery::new()
                .with_window(HlcTimestamp::new(0, 0), HlcTimestamp::new(100, 0))
                .matches(&record),
            "the upper bound is exclusive"
        );
    }

    #[test]
    fn log_queries_round_trip_and_describe_themselves() {
        let query = LogQuery::new()
            .with_min_level(LogLevel::Warn)
            .with_contains("queue")
            .with_target("astrs_daemon")
            .with_limit(Some(10))
            .with_window(HlcTimestamp::new(1, 0), HlcTimestamp::new(2, 0));
        assert_eq!(round_trip(&query).unwrap(), query);
        assert!(!query.is_unfiltered());
        let text = query.to_string();
        for needle in ["level", "since", "until", "containing", "target"] {
            assert!(text.contains(needle), "{text} is missing {needle}");
        }
    }

    #[test]
    fn topic_queries_compute_their_pacing() {
        assert_eq!(TopicQuery::new().min_interval_ms(), None);
        assert_eq!(
            TopicQuery::new()
                .with_max_rate_hz(Some(4))
                .min_interval_ms(),
            Some(250)
        );
        assert_eq!(
            TopicQuery::new()
                .with_max_rate_hz(Some(0))
                .min_interval_ms(),
            Some(u64::MAX),
            "a zero rate must not divide by zero"
        );
        assert!(TopicQuery::new().wants_payload());
        assert!(!TopicQuery::new().with_head_bytes(Some(0)).wants_payload());
        assert!(TopicQuery::new().with_head_bytes(Some(64)).wants_payload());
    }

    #[test]
    fn topic_queries_round_trip_and_describe_themselves() {
        let query = TopicQuery::new()
            .with_max_rate_hz(Some(2))
            .with_head_bytes(Some(32))
            .with_latest_only(true)
            .with_limit(Some(100))
            .with_metadata(false);
        assert_eq!(round_trip(&query).unwrap(), query);
        let text = query.to_string();
        for needle in ["Hz", "first", "latest only", "messages", "no metadata"] {
            assert!(text.contains(needle), "{text} is missing {needle}");
        }
        assert_eq!(TopicQuery::new().to_string(), "everything");
    }

    #[test]
    fn param_scopes_nest_and_fall_back() {
        let dataflow = DataflowId::from_u128(1);
        let node = ParamScope::node(dataflow, NodeId::new("camera").unwrap());
        assert_eq!(node.dataflow(), Some(dataflow));
        assert_eq!(node.node_id().map(NodeId::as_str), Some("camera"));
        assert_eq!(node.parent(), Some(ParamScope::dataflow_scope(dataflow)));

        let chain = node.lookup_chain();
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0], node);
        assert_eq!(chain[1], ParamScope::dataflow_scope(dataflow));
        assert_eq!(chain[2], ParamScope::Global);
        assert_eq!(ParamScope::Global.lookup_chain(), vec![ParamScope::Global]);
        assert_eq!(ParamScope::Global.dataflow(), None);
        assert_eq!(ParamScope::Global.node_id(), None);
    }

    #[test]
    fn param_scope_indices_are_frozen() {
        let scopes = [
            ParamScope::Global,
            ParamScope::dataflow_scope(DataflowId::from_u128(1)),
            ParamScope::node(DataflowId::from_u128(1), NodeId::new("camera").unwrap()),
        ];
        for (index, scope) in scopes.into_iter().enumerate() {
            assert_eq!(usize::from(scope.encode_to_vec().unwrap()[0]), index);
            assert_eq!(round_trip(&scope).unwrap(), scope);
            assert!(!scope.kind_name().is_empty());
            assert!(!scope.to_string().is_empty());
        }
    }

    #[test]
    fn error_code_indices_are_frozen_and_exit_codes_are_distinct() {
        let mut exits = std::collections::BTreeSet::new();
        for (index, code) in ErrorCode::ALL.iter().enumerate() {
            assert_eq!(usize::from(code.encode_to_vec().unwrap()[0]), index);
            assert!(exits.insert(code.exit_code()), "{code} reuses an exit code");
            assert_ne!(code.exit_code(), 0, "an error never exits successfully");
        }
        assert_eq!(ErrorCode::ALL.len(), 13);
        assert_eq!(ErrorCode::default(), ErrorCode::Internal);
    }

    #[test]
    fn error_codes_classify_themselves_consistently() {
        for &code in ErrorCode::ALL {
            // A caller error is never something to retry unchanged.
            assert!(
                !(code.is_caller_error() && code.is_retryable()),
                "{code} is both the caller's fault and retryable"
            );
            assert!(!code.as_str().is_empty());
        }
        assert!(ErrorCode::Unavailable.is_retryable());
        assert!(ErrorCode::NotFound.is_caller_error());
    }
}
