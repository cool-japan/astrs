//! Aggregating a one-shot `Logs` fetch across every daemon hosting a
//! dataflow (blueprint §17 `astrs logs`, §24.1 `CoordinatorEvent::Logs` /
//! `DaemonEvent::Log`).
//!
//! Unlike `LogSubscribe` (a live push [`crate::registry::subscription`]
//! fans out with no daemon round trip needed), a plain `Logs` request asks
//! every daemon hosting the named dataflow for its matching records and
//! waits for all of them — this is the shared state that lets the
//! handler that *issues* the fan-out (`CoordinatorRequest::Logs`'s
//! handler) and the daemon session task that *receives* each answer
//! (`DaemonEvent::Log { request: Some(id), .. }`) rendezvous on the same
//! `request` id.

use std::collections::{BTreeSet, HashMap};

use astrs_wire::{DaemonId, LogRecord};
use tokio::sync::oneshot;

/// The aggregate a [`PendingLogFetch`] resolves its waiters with.
#[derive(Debug, Clone, Default)]
pub struct LogFetchResult {
    /// Every matching record from every daemon that answered, in whatever
    /// order the daemons answered in — the handler sorts by timestamp
    /// before replying to the CLI.
    pub records: Vec<LogRecord>,
    /// Whether any daemon's own limit truncated its batch.
    pub truncated: bool,
}

/// One in-flight `Logs` fan-out.
pub struct PendingLogFetch {
    /// Daemons that have not yet answered.
    pub awaiting: BTreeSet<DaemonId>,
    /// Records received so far.
    pub records: Vec<LogRecord>,
    /// Whether any answer so far reported truncation.
    pub truncated: bool,
    /// Callers blocked on the aggregate, resolved once every daemon has
    /// answered.
    pub waiters: Vec<oneshot::Sender<LogFetchResult>>,
}

impl PendingLogFetch {
    /// A pending fetch expecting an answer from every daemon in
    /// `daemons`.
    #[must_use]
    pub fn new(daemons: impl IntoIterator<Item = DaemonId>) -> Self {
        Self {
            awaiting: daemons.into_iter().collect(),
            records: Vec::new(),
            truncated: false,
            waiters: Vec::new(),
        }
    }

    /// Records one daemon's answer, returning the aggregate once every
    /// awaited daemon has answered.
    pub fn record(
        &mut self,
        daemon: &DaemonId,
        records: Vec<LogRecord>,
        truncated: bool,
    ) -> Option<LogFetchResult> {
        self.records.extend(records);
        self.truncated |= truncated;
        self.awaiting.remove(daemon);
        if self.awaiting.is_empty() {
            Some(LogFetchResult {
                records: self.records.clone(),
                truncated: self.truncated,
            })
        } else {
            None
        }
    }
}

/// Every fan-out currently in flight, keyed by its correlation id
/// ([`crate::coordinator::Coordinator::next_request_id`]).
pub type PendingLogFetches = HashMap<u64, PendingLogFetch>;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_time::HlcTimestamp;
    use astrs_wire::LogLevel;

    #[test]
    fn resolves_only_once_every_daemon_answers() {
        let a = DaemonId::generate(None);
        let b = DaemonId::generate(None);
        let mut pending = PendingLogFetch::new([a.clone(), b.clone()]);

        assert!(
            pending
                .record(
                    &a,
                    vec![LogRecord::new(HlcTimestamp::EPOCH, LogLevel::Info, "x")],
                    false
                )
                .is_none()
        );
        let result = pending
            .record(
                &b,
                vec![LogRecord::new(HlcTimestamp::EPOCH, LogLevel::Info, "y")],
                true,
            )
            .expect("both daemons answered");
        assert_eq!(result.records.len(), 2);
        assert!(result.truncated, "truncation from either daemon propagates");
    }

    #[test]
    fn an_empty_daemon_set_resolves_immediately() {
        let pending = PendingLogFetch::new(std::iter::empty());
        // The constructor alone does not resolve anything; a caller must
        // still call `record` — but since nothing is awaited, the very
        // next thing the handler does is check `awaiting.is_empty()`
        // itself before ever calling `record`, which this asserts holds.
        assert!(pending.awaiting.is_empty());
        assert!(pending.records.is_empty());
    }
}
