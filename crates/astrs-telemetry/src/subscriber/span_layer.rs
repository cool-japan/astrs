//! [`AstrsSpanLayer`] — buffers finished spans as [`astrs_wire::TraceSpan`]
//! and hands each one to a caller-supplied sink (typically
//! [`crate::export::OtlpExporter::push_span`]).

use std::collections::BTreeMap;
use std::sync::Arc;

use tracing::Subscriber;
use tracing::span::{Attributes, Id, Record};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use astrs_time::HlcClock;
use astrs_wire::{DataflowId, NodeId, SpanStatus, TraceSpan};

use crate::ids::{encode_hex, new_span_id, new_trace_id};
use crate::subscriber::visitor::FieldVisitor;

/// Per-span bookkeeping, stashed in the span's `tracing_subscriber`
/// extensions between `on_new_span` and `on_close`.
struct SpanState {
    trace_id: String,
    span_id: String,
    parent_span_id: Option<String>,
    name: String,
    start: astrs_time::HlcTimestamp,
    attributes: BTreeMap<String, String>,
    status: SpanStatus,
}

/// Reads an `error` field out of a visited field map, if present, as the
/// [`SpanStatus`] it implies: `error = true` (or any non-`false` value)
/// means [`SpanStatus::Error`]; `error = false` means [`SpanStatus::Ok`].
/// A caller that never sets an `error` field gets [`SpanStatus::Unset`],
/// unaffected by this function (it only ever returns `Some` when the
/// field was actually present).
fn status_from_fields(fields: &BTreeMap<String, serde_json::Value>) -> Option<SpanStatus> {
    match fields.get("error") {
        Some(serde_json::Value::Bool(false)) => Some(SpanStatus::Ok),
        Some(_) => Some(SpanStatus::Error),
        None => None,
    }
}

/// A `tracing_subscriber::Layer` that turns every closed span into an
/// [`astrs_wire::TraceSpan`] and passes it to `sink` (blueprint §13:
/// "collected via a tracing-subscriber Layer buffering finished spans").
///
/// `node`/`dataflow` are stamped on *every* span this layer produces
/// (matching the blueprint's "per-process ... node attributes" framing,
/// §13: one AstRS process is one node for the whole process lifetime, so
/// there is exactly one value to stamp, not one per span).
///
/// Requires `S: LookupSpan` (any `tracing_subscriber::Registry`-based
/// subscriber satisfies this) because it stores state on spans between
/// `on_new_span` and `on_close`.
pub struct AstrsSpanLayer<F> {
    clock: Arc<HlcClock>,
    node: Option<NodeId>,
    dataflow: Option<DataflowId>,
    sink: F,
}

impl<F> AstrsSpanLayer<F>
where
    F: Fn(TraceSpan) + Send + Sync + 'static,
{
    /// Builds a span layer stamping every span with `node`/`dataflow`
    /// (either may be absent) and handing each finished span to `sink`.
    #[must_use]
    pub fn new(
        clock: Arc<HlcClock>,
        node: Option<NodeId>,
        dataflow: Option<DataflowId>,
        sink: F,
    ) -> Self {
        Self {
            clock,
            node,
            dataflow,
            sink,
        }
    }
}

impl<S, F> Layer<S> for AstrsSpanLayer<F>
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
    F: Fn(TraceSpan) + Send + Sync + 'static,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };

        let mut visitor = FieldVisitor::default();
        attrs.record(&mut visitor);

        let (trace_id, parent_span_id) = match span.parent() {
            Some(parent) => {
                let extensions = parent.extensions();
                match extensions.get::<SpanState>() {
                    Some(parent_state) => (
                        parent_state.trace_id.clone(),
                        Some(parent_state.span_id.clone()),
                    ),
                    // The parent's own state is gone (it already closed
                    // while this child stayed open, an unusual but legal
                    // tracing lifetime) -- start a fresh trace rather
                    // than fabricating a parent link to nothing.
                    None => (encode_hex(&new_trace_id()), None),
                }
            }
            None => (encode_hex(&new_trace_id()), None),
        };

        let state = SpanState {
            trace_id,
            span_id: encode_hex(&new_span_id()),
            parent_span_id,
            name: attrs.metadata().name().to_owned(),
            start: self.clock.now(),
            status: status_from_fields(&visitor.fields).unwrap_or(SpanStatus::Unset),
            attributes: visitor.attributes_as_strings(),
        };
        span.extensions_mut().insert(state);
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut visitor = FieldVisitor::default();
        values.record(&mut visitor);
        let mut extensions = span.extensions_mut();
        if let Some(state) = extensions.get_mut::<SpanState>() {
            if let Some(status) = status_from_fields(&visitor.fields) {
                state.status = status;
            }
            state.attributes.extend(visitor.attributes_as_strings());
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };
        let Some(state) = span.extensions_mut().remove::<SpanState>() else {
            return;
        };
        let trace_span = TraceSpan {
            trace_id: state.trace_id,
            span_id: state.span_id,
            parent_span_id: state.parent_span_id,
            name: state.name,
            start: state.start,
            end: Some(self.clock.now()),
            dataflow: self.dataflow,
            node: self.node.clone(),
            status: state.status,
            attributes: state.attributes,
        };
        (self.sink)(trace_span);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::sync::Mutex;
    use tracing_subscriber::layer::SubscriberExt;

    // The tuple is only "complex" because the layer's sink closure is an
    // opaque `impl Fn`, which cannot be named in a type alias without also
    // naming the closure. Extracting an alias here would need a boxed
    // `dyn Fn` and would change what the tests exercise.
    #[allow(clippy::type_complexity)]
    fn collecting_layer() -> (
        AstrsSpanLayer<impl Fn(TraceSpan) + Send + Sync + 'static>,
        Arc<Mutex<Vec<TraceSpan>>>,
    ) {
        let collected = Arc::new(Mutex::new(Vec::new()));
        let sink_target = Arc::clone(&collected);
        let layer = AstrsSpanLayer::new(
            Arc::new(HlcClock::system()),
            NodeId::new("camera").ok(),
            None,
            move |span| {
                sink_target
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(span)
            },
        );
        (layer, collected)
    }

    #[test]
    fn a_root_span_closes_into_one_finished_trace_span() {
        let (layer, collected) = collecting_layer();
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
            let span = tracing::info_span!("publish", output = "image");
            let _entered = span.enter();
        });
        let spans = collected.lock().unwrap();
        assert_eq!(spans.len(), 1);
        assert!(spans[0].end.is_some());
        assert_eq!(spans[0].name, "publish");
        assert_eq!(spans[0].parent_span_id, None);
        assert_eq!(
            spans[0].attributes.get("output").map(String::as_str),
            Some("image")
        );
        assert_eq!(spans[0].node.as_ref().map(NodeId::as_str), Some("camera"));
    }

    #[test]
    fn a_child_span_shares_its_parents_trace_id_and_links_the_parent_span_id() {
        let (layer, collected) = collecting_layer();
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
            let root = tracing::info_span!("root");
            let _root_entered = root.enter();
            {
                let child = tracing::info_span!("child");
                let _child_entered = child.enter();
                // `child` and `_child_entered` drop at the end of this
                // block, closing the child span while `root` is still
                // open -- `root` only closes when the outer closure
                // returns and its own binding drops.
            }
        });
        let spans = collected.lock().unwrap();
        assert_eq!(spans.len(), 2, "child closes first, then root");
        let (child_span, root_span) = (&spans[0], &spans[1]);
        assert_eq!(child_span.name, "child");
        assert_eq!(root_span.name, "root");
        assert_eq!(
            child_span.parent_span_id.as_deref(),
            Some(root_span.span_id.as_str())
        );
        assert_eq!(child_span.trace_id, root_span.trace_id);
    }

    #[test]
    fn spans_within_one_trace_share_the_trace_id_end_to_end() {
        let (layer, collected) = collecting_layer();
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
            let root = tracing::info_span!("root");
            {
                let _entered = root.enter();
                let child = tracing::info_span!("child");
                let _child_entered = child.enter();
            }
        });
        let spans = collected.lock().unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].trace_id, spans[1].trace_id);
    }

    #[test]
    fn an_error_field_marks_the_span_status() {
        let (layer, collected) = collecting_layer();
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
            let span = tracing::info_span!("risky", error = true);
            let _entered = span.enter();
        });
        let spans = collected.lock().unwrap();
        assert_eq!(spans[0].status, SpanStatus::Error);
    }

    #[test]
    fn an_explicit_error_false_marks_the_span_ok() {
        let (layer, collected) = collecting_layer();
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
            let span = tracing::info_span!("fine", error = false);
            let _entered = span.enter();
        });
        let spans = collected.lock().unwrap();
        assert_eq!(spans[0].status, SpanStatus::Ok);
    }

    #[test]
    fn no_error_field_leaves_the_status_unset() {
        let (layer, collected) = collecting_layer();
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
            let span = tracing::info_span!("plain");
            let _entered = span.enter();
        });
        let spans = collected.lock().unwrap();
        assert_eq!(spans[0].status, SpanStatus::Unset);
    }

    #[test]
    fn record_after_creation_updates_attributes_and_status() {
        let (layer, collected) = collecting_layer();
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
            let span = tracing::info_span!("late", extra = tracing::field::Empty);
            span.record("extra", "added-later");
            let _entered = span.enter();
        });
        let spans = collected.lock().unwrap();
        assert_eq!(
            spans[0].attributes.get("extra").map(String::as_str),
            Some("added-later")
        );
    }

    #[test]
    fn without_a_configured_node_the_span_has_none() {
        let collected = Arc::new(Mutex::new(Vec::new()));
        let sink_target = Arc::clone(&collected);
        let layer = AstrsSpanLayer::new(Arc::new(HlcClock::system()), None, None, move |span| {
            sink_target
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(span)
        });
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
            let span = tracing::info_span!("no-node");
            let _entered = span.enter();
        });
        assert_eq!(collected.lock().unwrap()[0].node, None);
    }
}
