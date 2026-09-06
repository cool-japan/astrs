//! `OutputSink` — the runtime host's shared, thread-safe front door onto
//! the connected node's own outputs.
//!
//! [`astrs_node_api::RawOutput::send_bytes`] takes `&mut self` — one writer
//! per output, matching `astrs-shm`'s own single-producer ring rule (§6.2).
//! Every hosted operator's thread may need to publish on the *same*
//! node-level output at once (blueprint §9.3's output mux: "operator sends
//! -> node outputs"), so this collects every
//! [`astrs_node_api::RawOutput`] the node declares behind its own
//! [`std::sync::Mutex`] once, up front, and hands operator threads a
//! `&OutputSink` — never a second `&mut Node` — for the rest of the run.

use std::collections::HashMap;
use std::sync::Mutex;

use astrs_node_api::{Node, NodeError, RawOutput};
use astrs_wire::{DataId, Metadata};

/// One [`astrs_node_api::RawOutput`] handle per output the connected node
/// declares, each behind its own lock.
pub(crate) struct OutputSink {
    outputs: HashMap<DataId, Mutex<RawOutput>>,
}

impl core::fmt::Debug for OutputSink {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut ids: Vec<&DataId> = self.outputs.keys().collect();
        ids.sort_unstable();
        f.debug_struct("OutputSink").field("outputs", &ids).finish()
    }
}

impl OutputSink {
    /// Opens a handle for every output `node` declares.
    ///
    /// # Errors
    ///
    /// [`astrs_node_api::NodeError::UnknownOutput`] should never fire here —
    /// every id comes from the node's own descriptor — but the call is
    /// fallible, so it is surfaced rather than assumed away.
    pub(crate) fn open(node: &mut Node) -> Result<Self, NodeError> {
        let ids: Vec<DataId> = node
            .descriptor()
            .outputs
            .iter()
            .map(|spec| spec.id.clone())
            .collect();
        let mut outputs = HashMap::with_capacity(ids.len());
        for id in ids {
            let raw = node.raw_output(id.as_str())?;
            outputs.insert(id, Mutex::new(raw));
        }
        Ok(Self { outputs })
    }

    /// Whether `id` is one of this node's declared outputs.
    #[must_use]
    pub(crate) fn declares(&self, id: &DataId) -> bool {
        self.outputs.contains_key(id)
    }

    /// Publishes `payload` on `id`.
    ///
    /// A no-op (not an error) when `id` is not declared — the routing
    /// table only calls this after checking [`OutputSink::declares`], but a
    /// second, cheap check here keeps this type sound to call from anywhere,
    /// not just from code that has already consulted the routing table.
    ///
    /// # Errors
    ///
    /// Whatever [`astrs_node_api::RawOutput::send_bytes`] reports.
    pub(crate) fn send(
        &self,
        id: &DataId,
        metadata: Metadata,
        payload: &[u8],
    ) -> Result<(), NodeError> {
        if !self.declares(id) {
            tracing::debug!(output = %id, "send on an output this node never declared; dropped");
            return Ok(());
        }
        let Some(output) = self.outputs.get(id) else {
            return Ok(());
        };
        output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .send_bytes(payload, metadata)
    }

    /// Closes every output.
    ///
    /// Called exactly once, strictly after every operator worker thread has
    /// joined (blueprint §9.3's shutdown ordering: "drain channels -> `on_stop`
    /// each operator -> close outputs") — a `on_stop` that flushes a final
    /// message must still find its output open.
    pub(crate) fn close_all(&self) {
        for output in self.outputs.values() {
            let _ignored = output
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .close();
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_node_api::testing::TestHarness;
    use astrs_time::HlcTimestamp;
    use std::time::Duration;

    #[test]
    fn opens_a_handle_for_every_declared_output() {
        let mut harness = TestHarness::start().unwrap();
        let sink = OutputSink::open(&mut harness.node).unwrap();
        assert!(sink.declares(&DataId::new(TestHarness::DEFAULT_OUTPUT).unwrap()));
        assert!(!sink.declares(&DataId::new("nope").unwrap()));
    }

    #[test]
    fn sending_on_a_declared_output_reaches_the_daemon() {
        let mut harness = TestHarness::start().unwrap();
        let node_id = harness.node.id().clone();
        let output_id = DataId::new(TestHarness::DEFAULT_OUTPUT).unwrap();
        let sink = OutputSink::open(&mut harness.node).unwrap();

        sink.send(&output_id, Metadata::new(HlcTimestamp::EPOCH), &[1, 2, 3])
            .unwrap();
        let sends = harness
            .daemon
            .wait_for_sends(&node_id, &output_id, 1, Duration::from_secs(5))
            .unwrap();
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].bytes(), Some(&[1, 2, 3][..]));
    }

    #[test]
    fn sending_on_an_undeclared_output_is_a_harmless_no_op() {
        let mut harness = TestHarness::start().unwrap();
        let sink = OutputSink::open(&mut harness.node).unwrap();
        sink.send(&DataId::new("nope").unwrap(), Metadata::default(), &[1])
            .unwrap();
    }

    #[test]
    fn close_all_closes_every_output_exactly_once() {
        let mut harness = TestHarness::start().unwrap();
        let node_id = harness.node.id().clone();
        let output_id = DataId::new(TestHarness::DEFAULT_OUTPUT).unwrap();
        let sink = OutputSink::open(&mut harness.node).unwrap();
        sink.close_all();
        sink.close_all();

        harness
            .daemon
            .wait_for(Duration::from_secs(5), |requests| {
                requests.iter().any(|entry| {
                    entry.node == node_id
                        && matches!(&entry.request, astrs_wire::NodeRequest::OutputDone { output } if *output == output_id)
                })
            })
            .unwrap();
        let closes = harness
            .daemon
            .requests()
            .into_iter()
            .filter(|entry| {
                entry.node == node_id
                    && matches!(&entry.request, astrs_wire::NodeRequest::OutputDone { output } if *output == output_id)
            })
            .count();
        assert_eq!(closes, 1, "one OutputDone, however many close_all calls");
    }

    #[test]
    fn debug_rendering_lists_output_ids() {
        let mut harness = TestHarness::start().unwrap();
        let sink = OutputSink::open(&mut harness.node).unwrap();
        assert!(format!("{sink:?}").contains(TestHarness::DEFAULT_OUTPUT));
    }
}
