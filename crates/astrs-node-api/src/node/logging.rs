//! Node logging (blueprint §9.1's `log_{error..trace} / log_with_fields`).
//!
//! A node's logs go two places at once, and both matter:
//!
//! * **Over the wire**, as `kind = Log` frames carrying an
//!   [`astrs_wire::LogRecord`], so `astrs logs` and the TUI see them
//!   interleaved with every other node's by HLC order (§13).
//! * **Into `tracing`**, so a node that already instruments itself with
//!   `tracing` keeps its local subscriber, its spans, and its `RUST_LOG`.
//!
//! Sending only over the wire would make a node undebuggable without a
//! daemon; emitting only locally would make a dataflow undebuggable without
//! ssh. Doing both costs one extra macro call per record.
//!
//! # Stamping
//!
//! Every record carries this node's HLC reading, its node id and its dataflow
//! id before it leaves. That is what lets the merger in `astrs-log` order
//! records from machines whose wall clocks disagree.
//!
//! # A dropped log is not an error
//!
//! Logging must never take a node down, and a node that cannot log is usually
//! a node whose daemon has already gone. The `log_*` methods therefore return
//! `()`; [`Node::try_log`] is there for the rare caller that wants to know.

use std::collections::BTreeMap;

use astrs_wire::{LogLevel, LogRecord};

use crate::error::Result;
use crate::node::Node;
use crate::session::Outgoing;

impl Node {
    /// Emits a record at `level`.
    ///
    /// Never fails: see the module documentation.
    pub fn log(&self, level: LogLevel, message: impl Into<String>) {
        let _ = self.try_log(level, message, BTreeMap::new());
    }

    /// Emits a record at `level` with structured fields (§9.1
    /// `log_with_fields`).
    pub fn log_with_fields(
        &self,
        level: LogLevel,
        message: impl Into<String>,
        fields: BTreeMap<String, String>,
    ) {
        let _ = self.try_log(level, message, fields);
    }

    /// Emits an `ERROR` record.
    pub fn log_error(&self, message: impl Into<String>) {
        self.log(LogLevel::Error, message);
    }

    /// Emits a `WARN` record.
    pub fn log_warn(&self, message: impl Into<String>) {
        self.log(LogLevel::Warn, message);
    }

    /// Emits an `INFO` record.
    pub fn log_info(&self, message: impl Into<String>) {
        self.log(LogLevel::Info, message);
    }

    /// Emits a `DEBUG` record.
    pub fn log_debug(&self, message: impl Into<String>) {
        self.log(LogLevel::Debug, message);
    }

    /// Emits a `TRACE` record.
    pub fn log_trace(&self, message: impl Into<String>) {
        self.log(LogLevel::Trace, message);
    }

    /// Emits a record, reporting whether it reached the wire.
    ///
    /// The local `tracing` emission happens either way — a node whose daemon
    /// has gone still deserves its own logs.
    ///
    /// # Errors
    ///
    /// [`crate::NodeError::DaemonGone`] once the session has ended, and
    /// [`crate::NodeError::Backpressure`] when the writer is behind.
    pub fn try_log(
        &self,
        level: LogLevel,
        message: impl Into<String>,
        fields: BTreeMap<String, String>,
    ) -> Result<()> {
        let message = message.into();
        emit_locally(level, self.id().as_str(), &message, &fields);

        let mut record = LogRecord::new(self.hlc_now(), level, message)
            .with_node(self.id().clone())
            .with_dataflow(self.dataflow_id())
            .with_target(self.id().as_str());
        for (key, value) in fields {
            record = record.with_field(key, value)?;
        }
        let session = self.session();
        session.try_send(Outgoing::log(session.log_subscription, record))
    }

    /// Builds a record stamped with this node's identity, without sending it.
    ///
    /// For a caller that wants to batch, filter or record locally before the
    /// wire sees anything.
    #[must_use]
    pub fn log_record(&self, level: LogLevel, message: impl Into<String>) -> LogRecord {
        LogRecord::new(self.hlc_now(), level, message)
            .with_node(self.id().clone())
            .with_dataflow(self.dataflow_id())
            .with_target(self.id().as_str())
    }

    /// Sends an already-built record.
    ///
    /// # Errors
    ///
    /// As [`Node::try_log`].
    pub fn send_log(&self, record: LogRecord) -> Result<()> {
        let session = self.session();
        session.try_send(Outgoing::log(session.log_subscription, record))
    }
}

/// Mirrors a record into the process's own `tracing` subscriber.
///
/// The level has to be a compile-time constant for `tracing`'s macros, which
/// is why this is a match rather than a single call.
fn emit_locally(level: LogLevel, node: &str, message: &str, fields: &BTreeMap<String, String>) {
    // `fields` is rendered rather than expanded: `tracing`'s field set is
    // fixed at the call site, and a node's field names are not.
    let rendered = if fields.is_empty() {
        String::new()
    } else {
        fields
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    match level {
        LogLevel::Error => tracing::error!(node, fields = %rendered, "{message}"),
        LogLevel::Warn => tracing::warn!(node, fields = %rendered, "{message}"),
        LogLevel::Info => tracing::info!(node, fields = %rendered, "{message}"),
        LogLevel::Debug => tracing::debug!(node, fields = %rendered, "{message}"),
        // `LogLevel` is `#[non_exhaustive]`; anything finer than debug is
        // trace as far as a local subscriber is concerned.
        _ => tracing::trace!(node, fields = %rendered, "{message}"),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::testing::TestHarness;
    use std::time::Duration;

    fn wait_for_logs(harness: &TestHarness, count: usize) -> Vec<LogRecord> {
        for _ in 0..200 {
            let logs = harness.daemon.logs();
            if logs.len() >= count {
                return logs;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        harness.daemon.logs()
    }

    #[test]
    fn every_level_reaches_the_daemon() {
        let harness = TestHarness::start().unwrap();
        harness.node.log_error("boom");
        harness.node.log_warn("careful");
        harness.node.log_info("hello");
        harness.node.log_debug("details");
        harness.node.log_trace("everything");

        let logs = wait_for_logs(&harness, 5);
        assert_eq!(logs.len(), 5, "{logs:?}");
        let levels: Vec<LogLevel> = logs.iter().map(|record| record.level).collect();
        assert_eq!(
            levels,
            vec![
                LogLevel::Error,
                LogLevel::Warn,
                LogLevel::Info,
                LogLevel::Debug,
                LogLevel::Trace
            ]
        );
        assert_eq!(logs[0].message, "boom");
    }

    #[test]
    fn records_carry_the_nodes_identity_and_clock() {
        let harness = TestHarness::start().unwrap();
        harness.node.log_info("stamped");
        let logs = wait_for_logs(&harness, 1);
        let record = logs.first().unwrap();
        assert_eq!(
            record.node.as_ref().map(astrs_wire::NodeId::as_str),
            Some(TestHarness::DEFAULT_NODE)
        );
        assert_eq!(record.dataflow, Some(harness.daemon.dataflow()));
        assert_eq!(record.target, TestHarness::DEFAULT_NODE);
        assert!(record.timestamp > astrs_time::HlcTimestamp::EPOCH);
    }

    #[test]
    fn structured_fields_survive_the_wire() {
        let harness = TestHarness::start().unwrap();
        let mut fields = BTreeMap::new();
        let _ = fields.insert("frame".to_owned(), "42".to_owned());
        let _ = fields.insert("camera".to_owned(), "front".to_owned());
        harness
            .node
            .log_with_fields(LogLevel::Warn, "dropped a frame", fields);

        let logs = wait_for_logs(&harness, 1);
        let record = logs.first().unwrap();
        assert_eq!(record.field("frame"), Some("42"));
        assert_eq!(record.field("camera"), Some("front"));
    }

    #[test]
    fn a_prebuilt_record_can_be_sent_as_is() {
        let harness = TestHarness::start().unwrap();
        let record = harness.node.log_record(LogLevel::Info, "prebuilt");
        assert_eq!(record.message, "prebuilt");
        harness.node.send_log(record).unwrap();
        let logs = wait_for_logs(&harness, 1);
        assert_eq!(
            logs.first().map(|record| record.message.clone()),
            Some("prebuilt".to_owned())
        );
    }

    #[test]
    fn logging_after_shutdown_is_reported_but_not_fatal() {
        let mut harness = TestHarness::start().unwrap();
        harness.node.shutdown().unwrap();
        // The infallible face stays infallible.
        harness.node.log_error("after the end");
        let error = harness
            .node
            .try_log(LogLevel::Info, "after the end", BTreeMap::new())
            .unwrap_err();
        assert!(matches!(error, crate::NodeError::DaemonGone));
    }
}
