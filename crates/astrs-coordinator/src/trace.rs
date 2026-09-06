//! The coordinator's own request-span buffer (blueprint §13, §17):
//! `GetTraces`'s server-side half.
//!
//! Two producers feed [`TraceBuffer`], and both are real:
//!
//! 1. **Always on, self-contained.** Every [`ControlRequest`](astrs_wire::ControlRequest)
//!    this coordinator finishes handling becomes one real, causally-ordered
//!    [`TraceSpan`] via [`control_span`], pushed by [`crate::handlers::dispatch`]
//!    unconditionally — no `tracing` subscriber required. This is what makes
//!    a bare `Coordinator::open_in_memory()` (every test in this crate) have
//!    real spans to answer `GetTraces` with, filterable exactly the way it
//!    promises, rather than a handler that always answers empty regardless
//!    of its own arguments.
//! 2. **Genuinely astrs-telemetry-sourced, when a real process wires it.**
//!    `astrs-telemetry`'s span-collection pipeline (`AstrsSpanLayer`,
//!    `init_telemetry_with_spans`) is process-wide — one global `tracing`
//!    subscriber, installed once near a binary's `main` — so it cannot live
//!    in this library crate at all. [`Coordinator::trace_sink`] is the seam
//!    that lets a real process (`bins/astrs-cli::command::serve::coordinator`)
//!    install that subscriber with its span layer's sink pushing straight
//!    into this same buffer, and [`crate::handlers::dispatch`] enters a real
//!    `tracing::Span` around every request specifically so that seam has
//!    something to carry once it exists. `dispatch`'s own doc comment spells
//!    out exactly how the two compose (and the one place they overlap).
//!
//! [`Coordinator::trace_sink`]: crate::coordinator::Coordinator::trace_sink
//!
//! # What this does not claim
//!
//! A `publish → deliver → process` causal chain across nodes and daemons
//! (blueprint §13's own example) needs span context to travel in message
//! metadata and back over the wire — a real feature, explicitly out of
//! scope here ("full tracing UI is out of scope"). [`TraceBuffer`] only
//! ever holds root spans naming *this coordinator's* control-plane work.

use std::collections::VecDeque;

use astrs_time::HlcTimestamp;
use astrs_wire::{DataflowId, NodeId, SpanStatus, TraceData, TraceSpan};

/// How many spans [`TraceBuffer`] keeps before evicting the oldest.
///
/// A control-plane request every few milliseconds (the busiest this gets)
/// still takes minutes to fill this, and `astrs trace` is a live-debugging
/// tool, not an audit log — `.arec` recordings (blueprint §14) are the
/// durable story.
pub const TRACE_BUFFER_CAPACITY: usize = 512;

/// A bounded, oldest-first ring of the coordinator's own finished request
/// spans.
///
/// Push-only from the coordinator's side (every [`TraceSpan`] here is
/// already closed — `end.is_some()` — by the time it is pushed) and
/// read-only from [`crate::handlers::misc::get_traces`]'s side, which is
/// why this has no `pub` mutation beyond [`TraceBuffer::push`].
#[derive(Debug, Default)]
pub struct TraceBuffer {
    spans: VecDeque<TraceSpan>,
}

impl TraceBuffer {
    /// An empty buffer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            spans: VecDeque::with_capacity(TRACE_BUFFER_CAPACITY),
        }
    }

    /// Records one finished span, evicting the oldest once
    /// [`TRACE_BUFFER_CAPACITY`] is reached.
    pub fn push(&mut self, span: TraceSpan) {
        if self.spans.len() >= TRACE_BUFFER_CAPACITY {
            self.spans.pop_front();
        }
        self.spans.push_back(span);
    }

    /// How many spans are currently buffered — for tests; `GetTraces`
    /// callers use [`TraceBuffer::query`] instead.
    #[must_use]
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    /// Whether the buffer currently holds no spans.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// Answers a `GetTraces` request: every buffered span matching
    /// `dataflow`/`node`/`since` (each `None` matches everything), oldest
    /// first, capped at `limit` (`None` means unbounded).
    ///
    /// `limit` truncates rather than samples, so a caller polling with an
    /// advancing `since` never skips a span — only ever sees the same tail
    /// end repeated across two calls if it asks for fewer than arrived.
    #[must_use]
    pub fn query(
        &self,
        dataflow: Option<DataflowId>,
        node: Option<&NodeId>,
        since: Option<HlcTimestamp>,
        limit: Option<u32>,
    ) -> TraceData {
        let mut matched: Vec<TraceSpan> = self
            .spans
            .iter()
            .filter(|span| dataflow.is_none_or(|wanted| span.dataflow == Some(wanted)))
            .filter(|span| node.is_none_or(|wanted| span.node.as_ref() == Some(wanted)))
            .filter(|span| since.is_none_or(|floor| span.start >= floor))
            .cloned()
            .collect();
        let truncated = match limit {
            Some(limit) => {
                let limit = limit as usize;
                let over = matched.len() > limit;
                matched.truncate(limit);
                over
            }
            None => false,
        };
        TraceData {
            spans: matched,
            truncated,
        }
    }
}

/// Builds the span [`crate::handlers::dispatch`] records for one finished
/// control request.
///
/// `trace_id`/`span_id` share `correlation` (formatted as hex): every
/// dispatched request is its own root today — there is no parent to link
/// to, since nothing yet threads a caller's trace context into a
/// [`astrs_wire::ControlRequest`] — so a request's trace and its one span
/// are the same causal unit, and giving them the same id says that
/// honestly rather than fabricating a distinct trace id nothing else ever
/// refers to.
#[must_use]
pub fn control_span(
    name: &'static str,
    dataflow: Option<DataflowId>,
    correlation: u64,
    start: HlcTimestamp,
    end: HlcTimestamp,
    status: SpanStatus,
) -> TraceSpan {
    let id = format!("{correlation:016x}");
    let mut span = TraceSpan::new(id.clone(), id, name, start)
        .with_end(end)
        .with_status(status);
    span.dataflow = dataflow;
    span
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn span(
        name: &str,
        dataflow: Option<DataflowId>,
        node: Option<NodeId>,
        start: u64,
    ) -> TraceSpan {
        let mut span = TraceSpan::new("t", format!("s{start}"), name, HlcTimestamp::new(start, 0));
        span.dataflow = dataflow;
        span.node = node;
        span
    }

    #[test]
    fn a_fresh_buffer_answers_every_query_empty() {
        let buffer = TraceBuffer::new();
        assert!(buffer.is_empty());
        let data = buffer.query(None, None, None, None);
        assert!(data.spans.is_empty());
        assert!(!data.truncated);
    }

    #[test]
    fn pushed_spans_come_back_oldest_first() {
        let mut buffer = TraceBuffer::new();
        buffer.push(span("a", None, None, 1));
        buffer.push(span("b", None, None, 2));
        buffer.push(span("c", None, None, 3));
        let data = buffer.query(None, None, None, None);
        let names: Vec<&str> = data.spans.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn the_buffer_evicts_the_oldest_span_once_full() {
        let mut buffer = TraceBuffer::new();
        for i in 0..TRACE_BUFFER_CAPACITY + 3 {
            buffer.push(span("x", None, None, i as u64));
        }
        assert_eq!(buffer.len(), TRACE_BUFFER_CAPACITY);
        let data = buffer.query(None, None, None, None);
        // The first three (start = 0, 1, 2) must have been evicted.
        assert_eq!(data.spans[0].start, HlcTimestamp::new(3, 0));
    }

    #[test]
    fn a_dataflow_filter_only_matches_that_dataflow() {
        let mut buffer = TraceBuffer::new();
        let wanted = DataflowId::from_u128(1);
        let other = DataflowId::from_u128(2);
        buffer.push(span("in", Some(wanted), None, 1));
        buffer.push(span("out", Some(other), None, 2));
        buffer.push(span("global", None, None, 3));
        let data = buffer.query(Some(wanted), None, None, None);
        assert_eq!(data.spans.len(), 1);
        assert_eq!(data.spans[0].name, "in");
    }

    #[test]
    fn a_node_filter_only_matches_that_node() {
        let mut buffer = TraceBuffer::new();
        let camera = NodeId::new("camera").unwrap();
        let planner = NodeId::new("planner").unwrap();
        buffer.push(span("cam-span", None, Some(camera.clone()), 1));
        buffer.push(span("plan-span", None, Some(planner), 2));
        buffer.push(span("nodeless", None, None, 3));
        let data = buffer.query(None, Some(&camera), None, None);
        assert_eq!(data.spans.len(), 1);
        assert_eq!(data.spans[0].name, "cam-span");
    }

    #[test]
    fn a_since_filter_drops_anything_that_started_earlier() {
        let mut buffer = TraceBuffer::new();
        buffer.push(span("early", None, None, 1));
        buffer.push(span("late", None, None, 100));
        let data = buffer.query(None, None, Some(HlcTimestamp::new(50, 0)), None);
        assert_eq!(data.spans.len(), 1);
        assert_eq!(data.spans[0].name, "late");
    }

    #[test]
    fn a_limit_truncates_and_reports_it_was_truncated() {
        let mut buffer = TraceBuffer::new();
        for i in 0..5u64 {
            buffer.push(span("x", None, None, i));
        }
        let data = buffer.query(None, None, None, Some(2));
        assert_eq!(data.spans.len(), 2);
        assert!(data.truncated);
        // Still the oldest two, not an arbitrary two.
        assert_eq!(data.spans[0].start, HlcTimestamp::new(0, 0));
        assert_eq!(data.spans[1].start, HlcTimestamp::new(1, 0));
    }

    #[test]
    fn a_limit_that_is_not_reached_is_not_reported_as_truncated() {
        let mut buffer = TraceBuffer::new();
        buffer.push(span("x", None, None, 1));
        let data = buffer.query(None, None, None, Some(10));
        assert_eq!(data.spans.len(), 1);
        assert!(!data.truncated);
    }

    #[test]
    fn filters_combine_rather_than_override_each_other() {
        let mut buffer = TraceBuffer::new();
        let dataflow = DataflowId::from_u128(1);
        let node = NodeId::new("camera").unwrap();
        buffer.push(span("match", Some(dataflow), Some(node.clone()), 10));
        // Right dataflow, wrong node.
        buffer.push(span(
            "wrong-node",
            Some(dataflow),
            Some(NodeId::new("other").unwrap()),
            11,
        ));
        // Right node, wrong dataflow.
        buffer.push(span(
            "wrong-dataflow",
            Some(DataflowId::from_u128(2)),
            Some(node.clone()),
            12,
        ));
        // Too early.
        buffer.push(span("too-early", Some(dataflow), Some(node.clone()), 1));
        let data = buffer.query(
            Some(dataflow),
            Some(&node),
            Some(HlcTimestamp::new(5, 0)),
            None,
        );
        assert_eq!(data.spans.len(), 1);
        assert_eq!(data.spans[0].name, "match");
    }

    #[test]
    fn control_span_gives_the_same_id_to_its_trace_and_its_span() {
        let start = HlcTimestamp::new(1, 0);
        let end = HlcTimestamp::new(2, 0);
        let span = control_span("GetParam", None, 42, start, end, SpanStatus::Ok);
        assert_eq!(span.trace_id, span.span_id);
        assert!(span.is_root());
        assert_eq!(span.name, "GetParam");
        assert_eq!(span.status, SpanStatus::Ok);
        assert_eq!(span.end, Some(end));
    }

    #[test]
    fn control_span_stamps_the_dataflow_it_was_given() {
        let dataflow = DataflowId::from_u128(9);
        let span = control_span(
            "SetParam",
            Some(dataflow),
            1,
            HlcTimestamp::EPOCH,
            HlcTimestamp::EPOCH,
            SpanStatus::Ok,
        );
        assert_eq!(span.dataflow, Some(dataflow));
    }
}
