//! [`LogHistory`] — the bounded ring `astrs logs` reads from (blueprint §13,
//! §17).
//!
//! There are two log paths out of a daemon and they answer two different
//! questions:
//!
//! | Path | Question | Mechanism |
//! |---|---|---|
//! | push | *what is happening now?* | every record goes to the [`crate::health::ReportSink`] as `DaemonEvent::Log { request: None }` the moment it is produced, and the coordinator fans it out to live `astrs logs -f` subscribers |
//! | pull | *what happened?* | the coordinator sends [`astrs_wire::CoordinatorEvent::Logs`] and expects a batch back |
//!
//! The push path needs no memory at all. The pull path needs *some*, and this
//! is exactly how much: a ring of the most recent [`LogHistory::capacity`]
//! records, filtered on the way out by the caller's
//! [`astrs_wire::LogQuery`]. A daemon on a robot must not accumulate a week of
//! logs in RAM to answer a question nobody asked, and `astrs logs` on a live
//! cluster wants the recent tail, not an archive — the archive is the rotated
//! file `astrs-log` writes (§13).
//!
//! # HLC ordering across machines
//!
//! Records are stamped by the producing node's (or daemon's) hybrid logical
//! clock and kept in arrival order here. The coordinator sorts the union of
//! every daemon's answer by `timestamp` before replying (§13: *"merged
//! formatting for `astrs logs -f` … HLC-ordered across machines"*), which is
//! well defined precisely because the stamps are HLC rather than wall clock.
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::coordinator::LogHistory;
//! use astrs_time::HlcTimestamp;
//! use astrs_wire::{DataflowId, LogLevel, LogQuery, LogRecord};
//!
//! let mut history = LogHistory::new(2);
//! for seq in 0..3u64 {
//!     history.record(
//!         LogRecord::new(HlcTimestamp::new(seq, 0), LogLevel::Info, format!("line {seq}"))
//!             .with_dataflow(DataflowId::from_u128(1)),
//!     );
//! }
//!
//! let batch = history.query(Some(DataflowId::from_u128(1)), None, &LogQuery::new());
//! assert_eq!(batch.records.len(), 2, "the ring is bounded");
//! assert_eq!(batch.records[0].message, "line 1");
//! ```

use std::collections::VecDeque;

use astrs_wire::{DataflowId, LogQuery, LogRecord, NodeId};

/// How many records a daemon keeps for the pull path by default.
///
/// Four thousand records is a few seconds of a chatty graph and several
/// minutes of a quiet one — enough for `astrs logs` to explain a failure that
/// just happened, and small enough to be an unremarkable amount of memory on
/// an embedded target.
pub const DEFAULT_LOG_HISTORY: usize = 4096;

/// One answer to a [`astrs_wire::CoordinatorEvent::Logs`] request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogBatch {
    /// The matching records, oldest first.
    pub records: Vec<LogRecord>,
    /// Whether a limit cut the batch short.
    pub truncated: bool,
}

/// The most recent log records this daemon produced.
#[derive(Debug)]
pub struct LogHistory {
    /// The ring, oldest first.
    records: VecDeque<LogRecord>,
    /// How many it holds before it starts discarding.
    capacity: usize,
    /// How many were discarded, ever.
    discarded: u64,
}

impl LogHistory {
    /// A ring holding at most `capacity` records.
    ///
    /// A `capacity` of zero disables the pull path entirely — a legitimate
    /// configuration for a daemon whose logs are read from files only — and
    /// [`LogHistory::query`] then answers every request with an empty batch
    /// rather than pretending it lost something.
    #[must_use]
    pub const fn new(capacity: usize) -> Self {
        Self {
            records: VecDeque::new(),
            capacity,
            discarded: 0,
        }
    }

    /// How many records it holds before discarding.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// How many records are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether nothing is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// How many records the ring has discarded.
    #[must_use]
    pub const fn discarded(&self) -> u64 {
        self.discarded
    }

    /// Adds one record, discarding the oldest if the ring is full.
    pub fn record(&mut self, record: LogRecord) {
        if self.capacity == 0 {
            self.discarded = self.discarded.saturating_add(1);
            return;
        }
        while self.records.len() >= self.capacity {
            self.records.pop_front();
            self.discarded = self.discarded.saturating_add(1);
        }
        self.records.push_back(record);
    }

    /// Every record matching `dataflow`, `node` and `query`, oldest first.
    ///
    /// `dataflow`/`node` of [`None`] mean "any". The query's own
    /// [`LogQuery::limit`] is applied *after* filtering and keeps the
    /// **newest** matches, because a truncated answer to "what happened?" is
    /// far more useful ending at the failure than beginning at boot.
    #[must_use]
    pub fn query(
        &self,
        dataflow: Option<DataflowId>,
        node: Option<&NodeId>,
        query: &LogQuery,
    ) -> LogBatch {
        let mut matched: Vec<&LogRecord> = self
            .records
            .iter()
            .filter(|record| {
                dataflow.is_none_or(|wanted| record.dataflow == Some(wanted))
                    && node.is_none_or(|wanted| record.node.as_ref() == Some(wanted))
                    && query.matches(record)
            })
            .collect();

        let truncated = match query.limit {
            Some(limit) => {
                let limit = limit as usize;
                if matched.len() > limit {
                    matched.drain(..matched.len() - limit);
                    true
                } else {
                    false
                }
            }
            None => false,
        };

        LogBatch {
            records: matched.into_iter().cloned().collect(),
            truncated,
        }
    }

    /// Forgets every record.
    pub fn clear(&mut self) {
        self.records.clear();
    }
}

impl Default for LogHistory {
    fn default() -> Self {
        Self::new(DEFAULT_LOG_HISTORY)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_time::HlcTimestamp;
    use astrs_wire::LogLevel;

    use super::*;

    fn dataflow() -> DataflowId {
        DataflowId::from_u128(1)
    }

    fn record(seq: u64, level: LogLevel, node: &str) -> LogRecord {
        LogRecord::new(HlcTimestamp::new(seq, 0), level, format!("line {seq}"))
            .with_dataflow(dataflow())
            .with_node(NodeId::new(node).unwrap())
    }

    #[test]
    fn a_default_history_is_empty_and_bounded() {
        let history = LogHistory::default();
        assert!(history.is_empty());
        assert_eq!(history.capacity(), DEFAULT_LOG_HISTORY);
        assert_eq!(history.discarded(), 0);
    }

    #[test]
    fn the_ring_keeps_the_newest_records() {
        let mut history = LogHistory::new(3);
        for seq in 0..5 {
            history.record(record(seq, LogLevel::Info, "camera"));
        }
        assert_eq!(history.len(), 3);
        assert_eq!(history.discarded(), 2);
        let batch = history.query(None, None, &LogQuery::new());
        assert_eq!(batch.records[0].message, "line 2");
        assert_eq!(batch.records[2].message, "line 4");
        assert!(!batch.truncated);
    }

    #[test]
    fn a_zero_capacity_history_holds_nothing_and_says_so() {
        let mut history = LogHistory::new(0);
        history.record(record(1, LogLevel::Info, "camera"));
        assert!(history.is_empty());
        assert_eq!(history.discarded(), 1);
        assert!(
            history
                .query(None, None, &LogQuery::new())
                .records
                .is_empty()
        );
    }

    #[test]
    fn a_node_filter_selects_one_nodes_records() {
        let mut history = LogHistory::new(16);
        history.record(record(1, LogLevel::Info, "camera"));
        history.record(record(2, LogLevel::Info, "detect"));
        let camera = NodeId::new("camera").unwrap();
        let batch = history.query(Some(dataflow()), Some(&camera), &LogQuery::new());
        assert_eq!(batch.records.len(), 1);
        assert_eq!(batch.records[0].node.as_ref(), Some(&camera));
    }

    #[test]
    fn a_dataflow_filter_excludes_other_dataflows() {
        let mut history = LogHistory::new(16);
        history.record(record(1, LogLevel::Info, "camera"));
        let batch = history.query(Some(DataflowId::from_u128(9)), None, &LogQuery::new());
        assert!(batch.records.is_empty());
    }

    #[test]
    fn a_level_filter_is_applied() {
        let mut history = LogHistory::new(16);
        history.record(record(1, LogLevel::Debug, "camera"));
        history.record(record(2, LogLevel::Error, "camera"));
        let batch = history.query(None, None, &LogQuery::new().with_min_level(LogLevel::Warn));
        assert_eq!(batch.records.len(), 1);
        assert_eq!(batch.records[0].level, LogLevel::Error);
    }

    #[test]
    fn a_limit_keeps_the_newest_matches_and_reports_truncation() {
        let mut history = LogHistory::new(16);
        for seq in 0..5 {
            history.record(record(seq, LogLevel::Info, "camera"));
        }
        let batch = history.query(None, None, &LogQuery::new().with_limit(Some(2)));
        assert!(batch.truncated);
        assert_eq!(batch.records.len(), 2);
        assert_eq!(batch.records[0].message, "line 3");
        assert_eq!(batch.records[1].message, "line 4");
    }

    #[test]
    fn clearing_forgets_everything_but_not_the_counter() {
        let mut history = LogHistory::new(2);
        for seq in 0..4 {
            history.record(record(seq, LogLevel::Info, "camera"));
        }
        history.clear();
        assert!(history.is_empty());
        assert_eq!(history.discarded(), 2);
    }
}
