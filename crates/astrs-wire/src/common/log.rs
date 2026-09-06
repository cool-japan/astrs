//! Structured log records, and the level filter they are matched against.
//!
//! Blueprint §13: the `tracing` façade is used everywhere, `astrs-log`
//! provides rotation and the `astrs/logs/*` virtual-input fan-out, and
//! `astrs logs -f` merges records across machines in HLC order. That merge is
//! only possible because a record carries its own [`astrs_time::HlcTimestamp`]
//! rather than a local wall clock — two machines' wall clocks disagree, their
//! hybrid logical clocks do not.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{LogLevel, LogRecord, NodeId};
//! use astrs_time::HlcTimestamp;
//!
//! let record = LogRecord::new(HlcTimestamp::new(10, 0), LogLevel::Warn, "queue is full")
//!     .with_node(NodeId::new("detector")?)
//!     .with_target("astrs_scheduler")
//!     .with_field("queue", "images")?;
//!
//! assert!(record.level.is_enabled_at(LogLevel::Info));
//! assert_eq!(record.field("queue"), Some("images"));
//! # Ok::<(), astrs_wire::IdError>(())
//! ```

use core::fmt;
use core::str::FromStr;
use std::collections::BTreeMap;

use astrs_time::HlcTimestamp;
use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::error::{IdError, IdKind, preview};
use crate::ids::{DataflowId, NodeId};

/// The maximum number of structured fields accepted on one record.
///
/// Log records are fanned out to every subscriber; a record with ten thousand
/// fields is a denial-of-service vector, not a log line.
pub const MAX_LOG_FIELDS: usize = 64;

/// A log severity, ordered from most to least severe.
///
/// The [`Ord`] implementation follows *verbosity*, not severity:
/// `Error < Warn < Info < Debug < Trace`. That ordering is what makes a level
/// filter a simple comparison — a record passes a filter when its level is
/// less than or equal to the filter's.
///
/// # Examples
///
/// ```
/// use astrs_wire::LogLevel;
///
/// assert!(LogLevel::Error < LogLevel::Trace);
/// assert!(LogLevel::Warn.is_enabled_at(LogLevel::Info));
/// assert!(!LogLevel::Debug.is_enabled_at(LogLevel::Info));
/// assert_eq!("warn".parse::<LogLevel>()?, LogLevel::Warn);
/// # Ok::<(), astrs_wire::IdError>(())
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
pub enum LogLevel {
    /// Something failed and the operation could not continue.
    #[oxicode(variant = 0)]
    Error,
    /// Something is wrong but the operation continued.
    #[oxicode(variant = 1)]
    Warn,
    /// Normal lifecycle information. The default.
    #[default]
    #[oxicode(variant = 2)]
    Info,
    /// Detail useful when diagnosing a problem.
    #[oxicode(variant = 3)]
    Debug,
    /// Per-message detail; expensive, off by default.
    #[oxicode(variant = 4)]
    Trace,
}

impl LogLevel {
    /// Every level, from most to least severe.
    pub const ALL: &'static [Self] = &[
        Self::Error,
        Self::Warn,
        Self::Info,
        Self::Debug,
        Self::Trace,
    ];

    /// The lower-case name used in manifests, the CLI and `tracing`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }

    /// Whether a record at this level passes a `filter`-level threshold.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::LogLevel;
    ///
    /// assert!(LogLevel::Error.is_enabled_at(LogLevel::Error));
    /// assert!(LogLevel::Error.is_enabled_at(LogLevel::Trace));
    /// assert!(!LogLevel::Trace.is_enabled_at(LogLevel::Error));
    /// ```
    #[must_use]
    pub const fn is_enabled_at(self, filter: Self) -> bool {
        (self as u8) <= (filter as u8)
    }

    /// The severity as a small integer, `0` for [`LogLevel::Error`].
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for LogLevel {
    type Err = IdError;

    /// Parses a level name, case-insensitively.
    ///
    /// # Errors
    ///
    /// [`IdError::Malformed`] for an unknown name.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text.to_ascii_lowercase().as_str() {
            "error" | "err" => Ok(Self::Error),
            "warn" | "warning" => Ok(Self::Warn),
            "info" => Ok(Self::Info),
            "debug" => Ok(Self::Debug),
            "trace" => Ok(Self::Trace),
            _ => Err(IdError::Malformed {
                kind: IdKind::Name,
                value: preview(text),
                reason: "not a log level (error|warn|info|debug|trace)",
            }),
        }
    }
}

/// One structured log record.
///
/// # Examples
///
/// ```
/// use astrs_wire::{LogLevel, LogRecord};
/// use astrs_time::HlcTimestamp;
///
/// let record = LogRecord::new(HlcTimestamp::EPOCH, LogLevel::Error, "spawn failed");
/// assert_eq!(record.message, "spawn failed");
/// assert!(record.node.is_none());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct LogRecord {
    /// When the record was produced, on the producer's hybrid logical clock.
    pub timestamp: HlcTimestamp,
    /// The severity.
    pub level: LogLevel,
    /// The dataflow the record belongs to, if any.
    ///
    /// `None` for daemon- and coordinator-level records that precede or
    /// outlive any particular dataflow.
    pub dataflow: Option<DataflowId>,
    /// The node that produced the record, if any.
    pub node: Option<NodeId>,
    /// The `tracing` target — usually a module path.
    pub target: String,
    /// The formatted message.
    pub message: String,
    /// Structured key/value fields, in key order.
    pub fields: BTreeMap<String, String>,
}

impl LogRecord {
    /// A record with no dataflow, node, target or fields.
    #[must_use]
    pub fn new(timestamp: HlcTimestamp, level: LogLevel, message: impl Into<String>) -> Self {
        Self {
            timestamp,
            level,
            dataflow: None,
            node: None,
            target: String::new(),
            message: message.into(),
            fields: BTreeMap::new(),
        }
    }

    /// Attaches a dataflow id.
    #[must_use]
    pub fn with_dataflow(mut self, dataflow: DataflowId) -> Self {
        self.dataflow = Some(dataflow);
        self
    }

    /// Attaches a node id.
    #[must_use]
    pub fn with_node(mut self, node: NodeId) -> Self {
        self.node = Some(node);
        self
    }

    /// Sets the `tracing` target.
    #[must_use]
    pub fn with_target(mut self, target: impl Into<String>) -> Self {
        self.target = target.into();
        self
    }

    /// Adds a structured field.
    ///
    /// # Errors
    ///
    /// [`IdError::TooLong`] once [`MAX_LOG_FIELDS`] fields are present.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{LogLevel, LogRecord};
    /// use astrs_time::HlcTimestamp;
    ///
    /// let record = LogRecord::new(HlcTimestamp::EPOCH, LogLevel::Info, "ok")
    ///     .with_field("attempt", "3")?;
    /// assert_eq!(record.field("attempt"), Some("3"));
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    pub fn with_field(
        mut self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<Self, IdError> {
        let key = key.into();
        if !self.fields.contains_key(&key) && self.fields.len() >= MAX_LOG_FIELDS {
            return Err(IdError::TooLong {
                kind: IdKind::Name,
                len: self.fields.len() + 1,
                max: MAX_LOG_FIELDS,
            });
        }
        self.fields.insert(key, value.into());
        Ok(self)
    }

    /// Looks up a structured field.
    #[must_use]
    pub fn field(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(String::as_str)
    }

    /// Whether this record passes a level / node / dataflow filter.
    ///
    /// This is the predicate behind `astrs logs --level warn --node camera`
    /// and behind the `astrs/logs[/level[/node]]` virtual source (§8.4).
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{LogLevel, LogRecord, NodeId};
    /// use astrs_time::HlcTimestamp;
    ///
    /// let record = LogRecord::new(HlcTimestamp::EPOCH, LogLevel::Warn, "m")
    ///     .with_node(NodeId::new("camera")?);
    ///
    /// assert!(record.matches(LogLevel::Info, None, None));
    /// assert!(record.matches(LogLevel::Warn, Some(&NodeId::new("camera")?), None));
    /// assert!(!record.matches(LogLevel::Error, None, None));
    /// assert!(!record.matches(LogLevel::Trace, Some(&NodeId::new("other")?), None));
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    #[must_use]
    pub fn matches(
        &self,
        level: LogLevel,
        node: Option<&NodeId>,
        dataflow: Option<&DataflowId>,
    ) -> bool {
        if !self.level.is_enabled_at(level) {
            return false;
        }
        if let Some(node) = node
            && self.node.as_ref() != Some(node)
        {
            return false;
        }
        if let Some(dataflow) = dataflow
            && self.dataflow.as_ref() != Some(dataflow)
        {
            return false;
        }
        true
    }
}

impl fmt::Display for LogRecord {
    /// Renders one merged-log line: `<hlc> <LEVEL> <node> <target>: <message>`
    /// followed by the fields.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{LogLevel, LogRecord, NodeId};
    /// use astrs_time::HlcTimestamp;
    ///
    /// let record = LogRecord::new(HlcTimestamp::new(7, 0), LogLevel::Info, "hello")
    ///     .with_node(NodeId::new("camera")?);
    /// assert_eq!(record.to_string(), "7-0 INFO  camera: hello");
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {:<5}",
            self.timestamp,
            self.level.as_str().to_uppercase()
        )?;
        if let Some(node) = &self.node {
            write!(f, " {node}")?;
        }
        if !self.target.is_empty() {
            write!(f, " {}", self.target)?;
        }
        write!(f, ": {}", self.message)?;
        for (key, value) in &self.fields {
            write!(f, " {key}={value}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::codec::round_trip;

    #[test]
    fn levels_order_by_verbosity() {
        assert!(LogLevel::Error < LogLevel::Warn);
        assert!(LogLevel::Warn < LogLevel::Info);
        assert!(LogLevel::Info < LogLevel::Debug);
        assert!(LogLevel::Debug < LogLevel::Trace);
        assert_eq!(LogLevel::default(), LogLevel::Info);
    }

    #[test]
    fn level_filtering_is_inclusive_and_transitive() {
        for (index, level) in LogLevel::ALL.iter().copied().enumerate() {
            for (filter_index, filter) in LogLevel::ALL.iter().copied().enumerate() {
                assert_eq!(
                    level.is_enabled_at(filter),
                    index <= filter_index,
                    "{level} at {filter}"
                );
            }
            assert_eq!(level.as_u8() as usize, index);
        }
    }

    #[test]
    fn level_names_parse_case_insensitively_with_aliases() {
        assert_eq!("ERROR".parse::<LogLevel>().unwrap(), LogLevel::Error);
        assert_eq!("Err".parse::<LogLevel>().unwrap(), LogLevel::Error);
        assert_eq!("WARNING".parse::<LogLevel>().unwrap(), LogLevel::Warn);
        assert_eq!("TrAcE".parse::<LogLevel>().unwrap(), LogLevel::Trace);
        assert!("verbose".parse::<LogLevel>().is_err());
        assert!("".parse::<LogLevel>().is_err());
        for level in LogLevel::ALL.iter().copied() {
            assert_eq!(level.as_str().parse::<LogLevel>().unwrap(), level);
            assert_eq!(level.to_string(), level.as_str());
        }
    }

    #[test]
    fn record_builders_compose() {
        let record = LogRecord::new(HlcTimestamp::new(1, 0), LogLevel::Warn, "message")
            .with_dataflow(DataflowId::from_u128(1))
            .with_node(NodeId::new("n").unwrap())
            .with_target("astrs_daemon::spawn")
            .with_field("pid", "1234")
            .unwrap()
            .with_field("attempt", "2")
            .unwrap();

        assert_eq!(record.field("pid"), Some("1234"));
        assert_eq!(record.field("missing"), None);
        assert_eq!(record.fields.len(), 2);
        assert_eq!(record.target, "astrs_daemon::spawn");
        assert!(record.dataflow.is_some());
    }

    #[test]
    fn field_count_is_capped() {
        let mut record = LogRecord::new(HlcTimestamp::EPOCH, LogLevel::Info, "m");
        for index in 0..MAX_LOG_FIELDS {
            record = record.with_field(format!("k{index}"), "v").unwrap();
        }
        assert_eq!(record.fields.len(), MAX_LOG_FIELDS);
        // Replacing an existing key is still allowed at the cap.
        record = record.with_field("k0", "other").unwrap();
        assert_eq!(record.field("k0"), Some("other"));
        // Adding a new one is not.
        assert!(record.with_field("overflow", "v").is_err());
    }

    #[test]
    fn filtering_combines_level_node_and_dataflow() {
        let dataflow = DataflowId::from_u128(1);
        let other_dataflow = DataflowId::from_u128(2);
        let node = NodeId::new("camera").unwrap();
        let other_node = NodeId::new("planner").unwrap();

        let record = LogRecord::new(HlcTimestamp::EPOCH, LogLevel::Warn, "m")
            .with_node(node.clone())
            .with_dataflow(dataflow);

        assert!(record.matches(LogLevel::Warn, None, None));
        assert!(record.matches(LogLevel::Trace, Some(&node), Some(&dataflow)));
        assert!(!record.matches(LogLevel::Error, Some(&node), Some(&dataflow)));
        assert!(!record.matches(LogLevel::Trace, Some(&other_node), None));
        assert!(!record.matches(LogLevel::Trace, None, Some(&other_dataflow)));
    }

    #[test]
    fn a_record_without_a_node_matches_no_node_filter() {
        let record = LogRecord::new(HlcTimestamp::EPOCH, LogLevel::Info, "m");
        assert!(record.matches(LogLevel::Info, None, None));
        assert!(!record.matches(LogLevel::Info, Some(&NodeId::new("n").unwrap()), None));
    }

    #[test]
    fn display_is_a_merged_log_line() {
        let record = LogRecord::new(HlcTimestamp::new(7, 0), LogLevel::Info, "hello")
            .with_node(NodeId::new("camera").unwrap());
        assert_eq!(record.to_string(), "7-0 INFO  camera: hello");

        let bare = LogRecord::new(HlcTimestamp::new(7, 0), LogLevel::Error, "boom");
        assert_eq!(bare.to_string(), "7-0 ERROR: boom");

        let full = record
            .with_target("astrs_log")
            .with_field("a", "1")
            .unwrap();
        assert_eq!(full.to_string(), "7-0 INFO  camera astrs_log: hello a=1");
    }

    #[test]
    fn records_round_trip_through_the_codec() {
        let record = LogRecord::new(HlcTimestamp::new(9, 3), LogLevel::Debug, "text")
            .with_dataflow(DataflowId::from_u128(5))
            .with_node(NodeId::new("n").unwrap())
            .with_target("t")
            .with_field("k", "v")
            .unwrap();
        assert_eq!(round_trip(&record).unwrap(), record);

        let minimal = LogRecord::new(HlcTimestamp::EPOCH, LogLevel::Trace, String::new());
        assert_eq!(round_trip(&minimal).unwrap(), minimal);
    }

    #[test]
    fn levels_round_trip_and_freeze_their_indices() {
        use crate::codec::WireEncode;

        for (index, level) in LogLevel::ALL.iter().copied().enumerate() {
            assert_eq!(round_trip(&level).unwrap(), level);
            assert_eq!(level.encode_to_vec().unwrap()[0], index as u8);
        }
    }

    #[test]
    fn serde_uses_lower_case_level_names() {
        assert_eq!(serde_json::to_string(&LogLevel::Warn).unwrap(), "\"warn\"");
        assert_eq!(
            serde_json::from_str::<LogLevel>("\"trace\"").unwrap(),
            LogLevel::Trace
        );
    }
}
