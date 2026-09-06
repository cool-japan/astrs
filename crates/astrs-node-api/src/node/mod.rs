//! [`Node`] — the flagship surface of blueprint §9.1.
//!
//! ```no_run
//! use astrs_node_api::prelude::*;
//!
//! fn main() -> Result<(), NodeError> {
//!     let (mut node, mut events) = Node::init_from_env()?;
//!     let mut echo = node.raw_output("echo")?;
//!
//!     while let Some(event) = events.recv() {
//!         match event {
//!             Event::Input { id, data, meta } if id == "frames" => {
//!                 echo.send_bytes(data.to_vec(), meta.follow())?;
//!             }
//!             Event::Stop(_) => break,
//!             _ => {}
//!         }
//!     }
//!     Ok(())
//! }
//! ```
//!
//! # Four ways in
//!
//! | Constructor | For |
//! |---|---|
//! | [`Node::init_from_env`] | A node the daemon spawned (§24.2's `ASTRS_NODE_CONFIG`) |
//! | [`Node::builder`] | A node that wants to override the id, endpoint or token |
//! | [`Node::init_from_node_id`] | A `path: dynamic` node attaching itself (§8.3) |
//! | [`Node::init_testing`] | A unit test, against an in-process daemon |
//!
//! All four produce the same `(Node, EventStream)` pair, so a node's body does
//! not know which one started it — which is exactly what makes
//! [`Node::init_testing`] able to exercise the real thing.
//!
//! # Ownership
//!
//! [`Node`] owns the session; [`crate::EventStream`] owns the
//! reading end. Dropping the node closes its outputs and ends the session;
//! dropping the stream tells the daemon to stop queueing (§7.3
//! `EventStreamDropped`). Either can be dropped first.

pub mod builder;
pub mod extension;
pub mod init;
pub mod logging;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use astrs_data::AstrsMessage;
use astrs_time::HlcTimestamp;
use astrs_wire::{
    DataId, DataflowId, NodeId, NodeRequest, NodeSpawnSpec, PortRef, SessionId, StopCause,
};

use astrs_scheduler::QueueSnapshot;

use crate::env::TypeCheckMode;
use crate::error::{NodeError, Result};
use crate::events::EventStream;
use crate::events::source::StreamStats;
use crate::orphan::OrphanGuard;
use crate::output::{Output, RawOutput};
use crate::session::pump::SessionTasks;
use crate::session::{SessionShared, SessionStats};

/// How long [`Node`]'s drop waits for the writer to flush what it queued.
///
/// The messages are already in the outgoing channel, so this is a scheduling
/// wait rather than a transfer: it expires only if the writer is wedged or
/// the daemon has stopped reading, and in either case a departing node cannot
/// improve matters by waiting longer.
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(5);

pub use builder::NodeBuilder;
pub use init::Registration;

/// A live participant in a dataflow.
pub struct Node {
    /// The session this node speaks through.
    shared: Arc<SessionShared>,
    /// The reader and writer tasks.
    tasks: Option<SessionTasks>,
    /// How strictly typed handles check declared types (§9.2).
    type_check: TypeCheckMode,
    /// The orphan guard, when one was requested (§4.2).
    orphan: Option<OrphanGuard>,
    /// Which outputs have handed out a handle, so a second one is refused.
    opened: BTreeSet<DataId>,
}

impl core::fmt::Debug for Node {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Node")
            .field("id", &self.shared.node)
            .field("dataflow", &self.shared.dataflow)
            .field("generation", &self.shared.generation)
            .field("type_check", &self.type_check)
            .field("orphan_guard", &self.orphan.is_some())
            .finish_non_exhaustive()
    }
}

impl Node {
    /// Builds a node around a completed registration.
    #[must_use]
    pub fn from_registration(
        registration: Registration,
        type_check: TypeCheckMode,
        orphan: Option<OrphanGuard>,
    ) -> (Self, EventStream) {
        let Registration {
            shared,
            events,
            tasks,
            spec: _,
        } = registration;
        (
            Self {
                shared,
                tasks: Some(tasks),
                type_check,
                orphan,
                opened: BTreeSet::new(),
            },
            events,
        )
    }

    /// Joins a dataflow using the `ASTRS_NODE_CONFIG` blob (§24.2).
    ///
    /// # Errors
    ///
    /// [`NodeError::Config`] when the blob is missing or malformed, and
    /// whatever [`NodeBuilder::connect`] reports.
    pub fn init_from_env() -> Result<(Self, EventStream)> {
        NodeBuilder::from_env()?.connect()
    }

    /// A builder for a node that needs to override something.
    #[must_use]
    pub fn builder() -> NodeBuilder {
        NodeBuilder::new()
    }

    /// Attaches a `path: dynamic` node under `node_id` (§8.3).
    ///
    /// # Errors
    ///
    /// As [`NodeBuilder::connect`].
    pub fn init_from_node_id(node_id: impl AsRef<str>) -> Result<(Self, EventStream)> {
        NodeBuilder::new().node_id(node_id)?.dynamic(true).connect()
    }

    /// Spins an in-process mock daemon and joins it (§9.1's `init_testing`).
    ///
    /// # Errors
    ///
    /// [`NodeError::Testing`] when the harness cannot be started.
    pub fn init_testing() -> Result<crate::testing::TestHarness> {
        crate::testing::TestHarness::start()
    }

    // ---------------------------------------------------------------- ports

    /// A typed publishing handle for `output` (§9.1).
    ///
    /// # Errors
    ///
    /// [`NodeError::UnknownOutput`] when the node does not declare it or a
    /// handle is already open, and [`NodeError::TypeMismatch`] when the
    /// port's declared URN disagrees with `T` under
    /// `ASTRS_TYPE_CHECK=error` (§9.2).
    pub fn output<T: AstrsMessage>(&mut self, output: impl AsRef<str>) -> Result<Output<T>> {
        let id = DataId::new(output.as_ref())?;
        self.check_declared_type(&id, T::URN)?;
        Ok(Output::new(self.claim_output(id)?))
    }

    /// An untyped publishing handle for `output`.
    ///
    /// # Errors
    ///
    /// [`NodeError::UnknownOutput`] when the node does not declare it or a
    /// handle is already open.
    pub fn raw_output(&mut self, output: impl AsRef<str>) -> Result<RawOutput> {
        let id = DataId::new(output.as_ref())?;
        self.claim_output(id)
    }

    /// The producer port behind one of this node's inputs.
    ///
    /// `Event::Input` deliberately does not carry it — a node's input has one
    /// source in the manifest, so it is a property of the wiring rather than
    /// of the message.
    #[must_use]
    pub fn input_source(&self, input: impl AsRef<str>) -> Option<PortRef> {
        let id = DataId::new(input.as_ref()).ok()?;
        self.shared.input_source(&id)
    }

    /// Asks the daemon for the next event(s) explicitly (§7.3 `NextEvent`).
    ///
    /// The node API is push-based: a node that simply reads its stream never
    /// needs this, and the daemon delivers as messages arrive. A node running
    /// its *own* event loop — an operator host, a bridge draining another
    /// runtime — uses it to control when work arrives, which is exactly what
    /// the verb exists for.
    ///
    /// `max_batch` bounds how many events one answer may carry; `timeout`
    /// asks the daemon to answer with an empty batch rather than blocking
    /// forever.
    ///
    /// # Errors
    ///
    /// [`NodeError::DaemonGone`] once the session has ended.
    pub fn request_events(&self, max_batch: u32, timeout: Option<Duration>) -> Result<()> {
        self.shared.send_request(NodeRequest::NextEvent {
            timeout: timeout.map(astrs_wire::DurationMs::from_duration),
            max_batch: max_batch.max(1),
        })
    }

    /// [`Node::request_events`] with the protocol's default batch size.
    ///
    /// # Errors
    ///
    /// As [`Node::request_events`].
    pub fn request_next_event(&self) -> Result<()> {
        self.request_events(astrs_wire::DEFAULT_EVENT_BATCH, None)
    }

    /// Closes every output at once, which is what a node does as it exits
    /// (§7.3 `CloseOutputs`).
    ///
    /// # Errors
    ///
    /// [`NodeError::DaemonGone`] once the session has ended.
    pub fn close_outputs(&mut self) -> Result<()> {
        self.shared.send_request(NodeRequest::CloseOutputs {
            outputs: Vec::new(),
        })
    }

    // ------------------------------------------------------- introspection

    /// The node's id (§9.1).
    #[must_use]
    pub fn id(&self) -> &NodeId {
        &self.shared.node
    }

    /// The dataflow this node belongs to.
    #[must_use]
    pub fn dataflow_id(&self) -> DataflowId {
        self.shared.dataflow
    }

    /// The connection's session id (§7.2).
    #[must_use]
    pub fn session_id(&self) -> SessionId {
        self.shared.session
    }

    /// The node's effective specification, as the daemon assigned it.
    #[must_use]
    pub fn descriptor(&self) -> &NodeSpawnSpec {
        &self.shared.spec
    }

    /// Whether this process is a restart of a node that ran before (§12).
    #[must_use]
    pub fn is_restart(&self) -> bool {
        self.shared.generation > 0
    }

    /// How many times this node has been restarted; `0` on a first spawn.
    #[must_use]
    pub fn restart_count(&self) -> u64 {
        self.shared.generation
    }

    /// This node's incarnation counter — the same number as
    /// [`Node::restart_count`], under the name the wire uses.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.shared.generation
    }

    /// The next hybrid-logical-clock reading (§4.3).
    #[must_use]
    pub fn hlc_now(&self) -> HlcTimestamp {
        self.shared.hlc_now()
    }

    /// A fresh metadata block stamped with [`Node::hlc_now`].
    #[must_use]
    pub fn metadata(&self) -> astrs_wire::Metadata {
        astrs_wire::Metadata::new(self.hlc_now())
    }

    /// How strictly typed handles check declared types (§9.2).
    #[must_use]
    pub const fn type_check(&self) -> TypeCheckMode {
        self.type_check
    }

    /// A snapshot of the session's counters.
    #[must_use]
    pub fn stats(&self) -> SessionStats {
        self.shared.stats()
    }

    /// The inputs this node reads, in name order.
    #[must_use]
    pub fn input_ids(&self) -> Vec<DataId> {
        self.shared.source.input_ids()
    }

    /// A point-in-time reading of one input's queue (§11.2).
    ///
    /// The counters `astrs list` and the TUI show per node: depth, how many
    /// of those are eviction-immune, and how many messages the policy has
    /// dropped.
    #[must_use]
    pub fn queue_snapshot(&self, input: impl AsRef<str>) -> Option<QueueSnapshot> {
        let id = DataId::new(input.as_ref()).ok()?;
        self.shared.source.queue_snapshot(&id)
    }

    /// Every input's queue counters, in name order.
    #[must_use]
    pub fn queue_snapshots(&self) -> Vec<(DataId, QueueSnapshot)> {
        self.shared
            .source
            .input_ids()
            .into_iter()
            .filter_map(|id| {
                self.shared
                    .source
                    .queue_snapshot(&id)
                    .map(|snapshot| (id, snapshot))
            })
            .collect()
    }

    /// The event stream's own counters.
    #[must_use]
    pub fn stream_stats(&self) -> StreamStats {
        self.shared.source.stats()
    }

    /// Which plane every known output is publishing on (§6.3).
    ///
    /// The producer end of the route. [`Node::input_planes`] is the consumer
    /// end, and the two are kept apart because an id names an output *or* an
    /// input, never both, and a node with an `image` input and an `image`
    /// output would otherwise have two rows nobody could tell apart.
    #[must_use]
    pub fn route_planes(&self) -> Vec<(DataId, crate::session::RoutePlane)> {
        self.shared.routes.planes()
    }

    /// Which plane every input the daemon has spoken about is read from
    /// (§6.3).
    ///
    /// An input reaches [`RoutePlane::Shm`](crate::session::RoutePlane::Shm)
    /// on its own: the daemon says which segment feeds it, the session
    /// attaches, and the payloads that arrive are
    /// [`Payload::is_zero_copy`](crate::Payload::is_zero_copy). An input the
    /// daemon has never mentioned does not appear at all.
    #[must_use]
    pub fn input_planes(&self) -> Vec<(DataId, crate::session::RoutePlane)> {
        self.shared.inputs.planes()
    }

    /// Which plane one input is read from (§6.3).
    ///
    /// [`RoutePlane::Daemon`](crate::session::RoutePlane::Daemon) for an input
    /// that is not on a ring, including one this node does not declare.
    #[must_use]
    pub fn input_plane(&self, input: impl AsRef<str>) -> crate::session::RoutePlane {
        match DataId::new(input.as_ref()) {
            Ok(id) => self.shared.inputs.plane(&id),
            Err(_) => crate::session::RoutePlane::Daemon,
        }
    }

    /// What the consumer-side plane has done: attachments, samples read
    /// straight out of a ring, and the §6.2 fallbacks (§6.3, §13).
    #[must_use]
    pub fn input_plane_stats(&self) -> crate::session::InputPlaneStats {
        self.shared.inputs.stats()
    }

    /// The segment one input reads from, when it is on a ring (§6.2).
    ///
    /// The geometry the daemon chose — name, generation, slot count and slot
    /// size — which is what a probe needs to say whether the addresses it saw
    /// cycled through the ring.
    #[must_use]
    pub fn input_segment(&self, input: impl AsRef<str>) -> Option<astrs_wire::ShmSegmentSpec> {
        let id = DataId::new(input.as_ref()).ok()?;
        self.shared.inputs.segment(&id)
    }

    /// Whether the orphan guard has observed its parent's disappearance
    /// (§4.2).
    #[must_use]
    pub fn is_orphaned(&self) -> bool {
        self.orphan.as_ref().is_some_and(OrphanGuard::has_fired)
    }

    /// Whether the session has ended.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.shared.is_closed()
    }

    /// The shared session state, for the patterns and the testing harness.
    #[must_use]
    pub fn session(&self) -> &Arc<SessionShared> {
        &self.shared
    }

    // ------------------------------------------------------------ shutdown

    /// Closes every output and ends the session.
    ///
    /// Called by [`Drop`]; explicit when a node wants to observe the result.
    ///
    /// # Errors
    ///
    /// [`NodeError::DaemonGone`] when the session had already ended, which is
    /// not a failure so much as a statement of fact.
    pub fn shutdown(&mut self) -> Result<()> {
        if self.shared.is_closed() {
            return Ok(());
        }
        // Order matters: the close has to be *queued* before the sentinel,
        // and the sentinel before the session is marked closed — otherwise
        // `try_send` would refuse them and a consumer would never learn that
        // this producer finished.
        let result = self.close_outputs();
        let _ = self.shared.request_drain();
        self.shared.close();
        result
    }

    /// Waits for the session's tasks to finish, after [`Node::shutdown`].
    ///
    /// Used by the testing harness to make shutdown deterministic.
    pub async fn join(&mut self) {
        if let Some(tasks) = self.tasks.take() {
            tasks.join().await;
        }
    }

    /// This node's private `drain_writer`'s safe-to-call-from-anywhere
    /// twin: waits, asynchronously, for the writer task to finish
    /// sending everything already queued (typically right after
    /// [`Node::shutdown`] or a last `RawOutput::send_batch`), bounded by
    /// the same `SHUTDOWN_DRAIN` timeout.
    ///
    /// `drain_writer` exists for exactly one caller — [`Drop`] — which
    /// cannot `.await`, so on a current-thread runtime it can only bail
    /// out rather than risk blocking that runtime's one worker on the
    /// very task that has to do the flushing (see its own docs for the
    /// loss that leaves open: the M1 pipeline losing its last frame on
    /// one run in twenty). A caller that is *already* inside an async
    /// context — a node built with [`NodeBuilder::connect_async`], which
    /// is precisely a caller for whom blocking was never on the table —
    /// has no such excuse: awaiting the writer here is cooperative, not
    /// blocking, and closes the gap `drain_writer` cannot on exactly the
    /// runtime flavor that gap matters most on.
    ///
    /// Only the writer is awaited, matching `drain_writer`: the reader
    /// lives until the *daemon* closes the socket, and a caller flushing
    /// its own outputs has no reason to wait for that. Idempotent —
    /// calling this (or letting the node drop) again afterward is a
    /// no-op, since the tasks are taken once.
    pub async fn flush_outputs(&mut self) {
        let Some(tasks) = self.tasks.take() else {
            return;
        };
        let _ = tokio::time::timeout(SHUTDOWN_DRAIN, tasks.writer).await;
        // `tasks.reader` (not moved above) drops here, detaching rather
        // than aborting it — see `drain_writer`'s identical reasoning.
    }

    /// Waits for the writer to send everything [`Node::shutdown`] queued.
    ///
    /// [`Node::shutdown`] only *queues* the output closures and the drain
    /// sentinel; the writer task is what puts them on the socket, and nothing
    /// waited for it. A node that published and then returned from `main`
    /// therefore raced its own runtime shutdown — [`crate::runtime`]'s owned
    /// runtime detaches its workers rather than joining them — and messages
    /// it had successfully sent were lost before reaching the daemon.
    /// Observed as the M1 pipeline recording 11 of its 12 frames on roughly
    /// one run in twenty, with the daemon confirming it was never given the
    /// twelfth.
    ///
    /// Only the writer is awaited. The reader lives until the *daemon* closes
    /// the socket, and a departing node has no reason to wait for that.
    fn drain_writer(&mut self) {
        let Some(tasks) = self.tasks.take() else {
            return;
        };
        if !self.shared.runtime.can_block() {
            // This thread is a current-thread runtime's only worker, so
            // blocking here would deadlock the very task that has to do the
            // flushing. Dropping the handles detaches those tasks rather than
            // aborting them, which still leaves them able to finish.
            return;
        }
        let runtime = self.shared.runtime.clone();
        let _ = runtime.block_on("Node::drop", "Node::join", async move {
            let _ = tokio::time::timeout(SHUTDOWN_DRAIN, tasks.writer).await;
        });
    }

    /// The stop cause a node should report for `error`, or `None` when it is
    /// not an ending.
    #[must_use]
    pub fn stop_cause_of(error: &NodeError) -> Option<StopCause> {
        error.stop_cause()
    }

    // ------------------------------------------------------------- private

    /// Hands out a handle for `output`, refusing a second one.
    fn claim_output(&mut self, id: DataId) -> Result<RawOutput> {
        if !self.shared.spec.outputs.is_empty() && !self.shared.declares_output(&id) {
            return Err(NodeError::UnknownOutput { output: id });
        }
        if !self.opened.insert(id.clone()) {
            return Err(NodeError::UnknownOutput { output: id });
        }
        Ok(RawOutput::new(Arc::clone(&self.shared), id))
    }

    /// Applies the §9.2 type-check policy to a typed handle.
    fn check_declared_type(&self, id: &DataId, requested: &str) -> Result<()> {
        if !self.type_check.is_enabled() {
            return Ok(());
        }
        let Some(declared) = self
            .shared
            .spec
            .output(id)
            .and_then(|spec| spec.type_urn.as_ref())
        else {
            return Ok(());
        };
        if declared.base() == requested || declared.as_str() == requested {
            return Ok(());
        }
        let mismatch = NodeError::TypeMismatch {
            port: id.clone(),
            declared: declared.as_str().to_owned(),
            requested: requested.to_owned(),
        };
        if self.type_check.is_fatal() {
            return Err(mismatch);
        }
        tracing::warn!(
            node = %self.shared.node,
            port = %id,
            declared = %declared,
            requested = requested,
            "port type mismatch (ASTRS_TYPE_CHECK=warn)"
        );
        Ok(())
    }

    /// The default timeout reply-carrying calls use.
    pub(crate) const fn reply_timeout() -> Duration {
        crate::session::DEFAULT_REPLY_TIMEOUT
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.shutdown();
        self.drain_writer();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::message::Vector3;
    use crate::testing::TestHarness;

    #[test]
    fn introspection_reports_what_the_daemon_assigned() {
        let harness = TestHarness::start().unwrap();
        let node = &harness.node;
        assert_eq!(node.id().as_str(), TestHarness::DEFAULT_NODE);
        assert_eq!(node.dataflow_id(), harness.daemon.dataflow());
        assert!(!node.is_restart());
        assert_eq!(node.restart_count(), 0);
        assert_eq!(node.generation(), 0);
        assert!(!node.is_orphaned());
        assert!(!node.is_closed());
        assert_eq!(node.type_check(), TypeCheckMode::Warn);
        assert!(node.hlc_now() <= node.hlc_now());
        assert!(format!("{node:?}").contains(TestHarness::DEFAULT_NODE));
    }

    #[test]
    fn an_output_handle_is_handed_out_once() {
        let mut harness = TestHarness::start().unwrap();
        let first = harness.node.raw_output("out");
        assert!(first.is_ok());
        let second = harness.node.raw_output("out");
        assert!(matches!(second, Err(NodeError::UnknownOutput { .. })));
    }

    #[test]
    fn a_typed_handle_carries_its_urn() {
        let mut harness = TestHarness::start().unwrap();
        let output = harness
            .node
            .output::<Vector3>(TestHarness::DEFAULT_OUTPUT)
            .unwrap();
        assert_eq!(output.type_urn(), "std/geometry/v1/Vector3");
    }

    #[test]
    fn shutdown_is_idempotent_and_closes_the_session() {
        let mut harness = TestHarness::start().unwrap();
        harness.node.shutdown().unwrap();
        assert!(harness.node.is_closed());
        harness.node.shutdown().unwrap();
    }

    #[test]
    fn a_node_can_read_its_own_queue_counters() {
        let mut harness = TestHarness::start().unwrap();
        assert_eq!(
            harness.node.input_ids(),
            vec![astrs_wire::DataId::new(TestHarness::DEFAULT_INPUT).unwrap()]
        );
        let snapshot = harness
            .node
            .queue_snapshot(TestHarness::DEFAULT_INPUT)
            .expect("the queue");
        assert_eq!(snapshot.depth, 0);
        assert_eq!(snapshot.capacity, astrs_wire::DEFAULT_QUEUE_SIZE);
        assert!(harness.node.queue_snapshot("absent").is_none());
        assert_eq!(harness.node.queue_snapshots().len(), 1);
        assert_eq!(harness.node.stream_stats().delivered, 0);

        harness.feed(vec![1]).unwrap();
        let event = harness.next_event().unwrap();
        assert!(event.is_input());
        assert_eq!(harness.node.stream_stats().delivered, 1);
    }

    #[test]
    fn the_reply_timeout_is_the_session_default() {
        assert_eq!(Node::reply_timeout(), crate::session::DEFAULT_REPLY_TIMEOUT);
        assert_eq!(
            Node::stop_cause_of(&NodeError::Stopped),
            Some(StopCause::Requested)
        );
    }
}
