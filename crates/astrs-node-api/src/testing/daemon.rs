//! [`MockDaemon`] — the daemon side of §7.3, in this process.
//!
//! Blueprint §9.1 lists `init_testing (in-process daemon for unit tests)` as
//! part of the node API's surface. This is that daemon. It is **not** a stub:
//! it speaks the real greeting (§7.2), the real `NodeRequest`/`NodeEvent`
//! families (§7.3), over the real frame codec (§7.1), on both ends of a
//! `tokio::io::duplex` pair. A node under test cannot tell it from a socket,
//! which is the entire point — a test that passes here is a test of the code
//! that ships.
//!
//! ```text
//!   Node ──► real greeting ──► real frames ──► MockDaemon
//!                                                  │
//!                                        routing table from the specs
//!                                                  │
//!   another Node ◄── real NodeEvent frames ◄───────┘
//! ```
//!
//! # What it does
//!
//! | Request | Answer |
//! |---|---|
//! | `Register` | `Registered { spec, session }` |
//! | `Subscribe` | Recorded; inputs start flowing |
//! | `SendMessage` | Recorded, **and routed** to every consumer whose `InputSpec::source` names that port |
//! | `OutputDone` / `CloseOutputs` | `InputClosed` to those consumers |
//! | `ExtStore` / `ExtLoad` / `ExtDrop` | An in-memory extension table; `ExtLoad` answers `ExtValue` |
//! | `RouteUpgradeAck` | Recorded, so a test can assert the §6.3 handshake completed |
//!
//! The routing is what makes a *two-node* test possible: connect a producer
//! and a consumer whose input names the producer's port, and a `send` on one
//! arrives as an `Event::Input` on the other, through the wire, exactly as it
//! would through a daemon.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use astrs_transport::accept;
use astrs_wire::messages::control::types::ParamScope;
use astrs_wire::metadata::Parameter;
use astrs_wire::{
    Acceptor, AuthToken, DataId, DataflowId, ExtensionKey, FrameKind, FrameLimits, LogRecord,
    Metadata, NodeEvent, NodeId, NodeRequest, NodeSpawnSpec, OutputPayload, ParamKey, PortRef,
    RoleSet, RouteCloseReason, RouteDowngradeReason, SessionAssignment, SessionId, ShmSegmentSpec,
    StopCause, WireMessage,
};
use tokio::sync::mpsc;

use crate::error::{NodeError, Result};
use crate::node::NodeBuilder;
use crate::runtime::NodeRuntime;
use crate::session::connect::{NodeLink, wrap_stream};
use crate::signal::{Signal, WaitOutcome};

/// The duplex buffer each mock connection gets.
///
/// Generous on purpose: a test that fills it would be measuring the buffer
/// rather than the node.
pub const DUPLEX_BUFFER: usize = 1024 * 1024;

/// How long the mock greeting is allowed to take.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// One request the daemon received, with the node it came from.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    /// Which node sent it.
    pub node: NodeId,
    /// What it was.
    pub request: NodeRequest,
}

/// One message a node published.
#[derive(Debug, Clone)]
pub struct RecordedSend {
    /// Which node published it.
    pub node: NodeId,
    /// On which output.
    pub output: DataId,
    /// The metadata beside it.
    pub metadata: Metadata,
    /// The payload, inline or as a shared-memory reference.
    pub payload: OutputPayload,
}

impl RecordedSend {
    /// The payload bytes, when it was carried inline.
    #[must_use]
    pub fn bytes(&self) -> Option<&[u8]> {
        self.payload.bytes()
    }

    /// The port this message came from.
    #[must_use]
    pub fn port(&self) -> PortRef {
        PortRef::new(self.node.clone(), self.output.clone())
    }
}

/// The per-node state the mock daemon keeps.
struct NodeState {
    /// The specification the node registered with.
    spec: NodeSpawnSpec,
    /// The channel the serving task writes events from.
    events: mpsc::UnboundedSender<NodeEvent>,
    /// Whether the node has subscribed yet.
    subscribed: bool,
}

/// The daemon's shared state.
struct DaemonInner {
    /// The dataflow every node joins.
    dataflow: DataflowId,
    /// The token the greeting requires.
    auth: AuthToken,
    /// Connected nodes.
    nodes: Mutex<HashMap<NodeId, NodeState>>,
    /// The extension table (§7.3).
    extensions: Mutex<HashMap<String, Vec<u8>>>,
    /// Every request received, in order.
    requests: Mutex<Vec<RecordedRequest>>,
    /// Every message published, in order.
    sends: Mutex<Vec<RecordedSend>>,
    /// Every log record received.
    logs: Mutex<Vec<LogRecord>>,
    /// Wakes [`MockDaemon::wait_for`].
    signal: Signal,
    /// Hands out session ids.
    next_session: AtomicU64,
}

impl DaemonInner {
    /// Records a request and wakes the waiters.
    fn record_request(&self, node: &NodeId, request: &NodeRequest) {
        lock(&self.requests).push(RecordedRequest {
            node: node.clone(),
            request: request.clone(),
        });
        self.signal.notify();
    }

    /// Sends an event to one node.
    fn deliver(&self, node: &NodeId, event: NodeEvent) -> bool {
        let nodes = lock(&self.nodes);
        let Some(state) = nodes.get(node) else {
            return false;
        };
        state.events.send(event).is_ok()
    }

    /// Every `(node, input)` that reads from `port`.
    fn consumers_of(&self, port: &PortRef) -> Vec<(NodeId, DataId)> {
        lock(&self.nodes)
            .iter()
            .flat_map(|(id, state)| {
                state
                    .spec
                    .inputs
                    .iter()
                    .filter(|input| &input.source == port)
                    .map(move |input| (id.clone(), input.id.clone()))
            })
            .collect()
    }
}

/// An in-process daemon for unit tests.
#[derive(Clone)]
pub struct MockDaemon {
    /// The shared state.
    inner: Arc<DaemonInner>,
    /// The runtime the serving tasks run on.
    runtime: NodeRuntime,
}

impl core::fmt::Debug for MockDaemon {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MockDaemon")
            .field("dataflow", &self.inner.dataflow)
            .field("nodes", &self.node_ids())
            .field("requests", &lock(&self.inner.requests).len())
            .finish_non_exhaustive()
    }
}

impl MockDaemon {
    /// Starts a daemon on a runtime of its own choosing.
    ///
    /// # Errors
    ///
    /// [`NodeError::Runtime`] when no runtime is running and one cannot be
    /// built.
    pub fn start() -> Result<Self> {
        Self::start_with(
            NodeRuntime::acquire()?,
            DataflowId::from_u128(1),
            AuthToken::ZERO,
        )
    }

    /// Starts a daemon with an explicit runtime, dataflow and token.
    ///
    /// # Errors
    ///
    /// Nothing today; the signature is fallible so a future backing store can
    /// report a failure without a breaking change.
    pub fn start_with(runtime: NodeRuntime, dataflow: DataflowId, auth: AuthToken) -> Result<Self> {
        Ok(Self {
            inner: Arc::new(DaemonInner {
                dataflow,
                auth,
                nodes: Mutex::new(HashMap::new()),
                extensions: Mutex::new(HashMap::new()),
                requests: Mutex::new(Vec::new()),
                sends: Mutex::new(Vec::new()),
                logs: Mutex::new(Vec::new()),
                signal: Signal::new(),
                next_session: AtomicU64::new(1),
            }),
            runtime,
        })
    }

    /// The dataflow every node joins.
    #[must_use]
    pub fn dataflow(&self) -> DataflowId {
        self.inner.dataflow
    }

    /// The token the greeting requires.
    #[must_use]
    pub fn auth(&self) -> AuthToken {
        self.inner.auth.clone()
    }

    /// The runtime the serving tasks run on.
    #[must_use]
    pub fn runtime(&self) -> &NodeRuntime {
        &self.runtime
    }

    /// The nodes currently connected.
    #[must_use]
    pub fn node_ids(&self) -> Vec<NodeId> {
        let mut ids: Vec<NodeId> = lock(&self.inner.nodes).keys().cloned().collect();
        ids.sort_unstable();
        ids
    }

    /// Connects a node with the given specification.
    ///
    /// # Errors
    ///
    /// [`NodeError::Testing`] when the greeting or registration fails, plus
    /// whatever [`NodeBuilder::finish`] reports.
    pub fn connect_node(&self, spec: NodeSpawnSpec) -> Result<(crate::Node, crate::EventStream)> {
        let daemon = self.clone();
        let runtime = self.runtime.clone();
        runtime.block_on(
            "MockDaemon::connect_node",
            "MockDaemon::connect_node_async",
            async move { daemon.connect_node_async(spec).await },
        )?
    }

    /// The async form of [`MockDaemon::connect_node`].
    ///
    /// # Errors
    ///
    /// As [`MockDaemon::connect_node`].
    pub async fn connect_node_async(
        &self,
        spec: NodeSpawnSpec,
    ) -> Result<(crate::Node, crate::EventStream)> {
        let node_id = spec.node.clone();
        let (node_io, daemon_io) = tokio::io::duplex(DUPLEX_BUFFER);
        let (event_sender, event_receiver) = mpsc::unbounded_channel();

        lock(&self.inner.nodes).insert(
            node_id.clone(),
            NodeState {
                spec: spec.clone(),
                events: event_sender,
                subscribed: false,
            },
        );

        let session = SessionId::from_u128(u128::from(
            self.inner.next_session.fetch_add(1, Ordering::Relaxed),
        ));
        let inner = Arc::clone(&self.inner);
        let served = node_id.clone();
        let sender = lock(&inner.nodes)
            .get(&served)
            .map(|state| state.events.clone());
        let _task = self.runtime.spawn(async move {
            let Some(sender) = sender else { return };
            serve(
                inner,
                daemon_io,
                Some(served),
                sender,
                event_receiver,
                session,
            )
            .await;
        });

        let mut duplex = wrap_stream(node_io, FrameLimits::uds());
        let params = crate::session::connect::node_handshake_params(
            self.inner.auth.clone(),
            Some(node_id.as_str().to_owned()),
        );
        let greeting = astrs_transport::initiate(&mut duplex, &params, HANDSHAKE_TIMEOUT)
            .await
            .map_err(|error| NodeError::Testing(format!("mock greeting failed: {error}")))?;

        let builder = NodeBuilder::new()
            .node_id(node_id.as_str())?
            .dataflow(self.inner.dataflow)
            .orphan_guard(false)
            .type_check(crate::env::TypeCheckMode::Warn)
            .zero_copy_threshold(astrs_wire::DEFAULT_ZERO_COPY_THRESHOLD);
        let handshake =
            astrs_wire::NodeHandshake::new(self.inner.dataflow, node_id, spec.generation)
                .with_pid(std::process::id());
        let link = NodeLink {
            duplex,
            session: greeting.session,
            endpoint: "mock://in-process".to_owned(),
        };
        builder.finish(link, handshake, self.runtime.clone()).await
    }

    // ------------------------------------------------------------- driving

    /// Sends an arbitrary event to a node.
    ///
    /// # Errors
    ///
    /// [`NodeError::Testing`] when no such node is connected.
    pub fn send_event(&self, node: &NodeId, event: NodeEvent) -> Result<()> {
        if self.inner.deliver(node, event) {
            Ok(())
        } else {
            Err(NodeError::Testing(format!("no node `{node}` is connected")))
        }
    }

    /// Delivers one input message to a node.
    ///
    /// # Errors
    ///
    /// As [`MockDaemon::send_event`].
    pub fn send_input(
        &self,
        node: &NodeId,
        input: &DataId,
        metadata: Metadata,
        payload: Vec<u8>,
    ) -> Result<()> {
        let source = lock(&self.inner.nodes)
            .get(node)
            .and_then(|state| state.spec.input(input).map(|spec| spec.source.clone()))
            .unwrap_or_else(|| PortRef::new(node.clone(), input.clone()));
        self.send_event(
            node,
            NodeEvent::Input {
                id: input.clone(),
                source,
                metadata,
                payload,
            },
        )
    }

    /// Closes one of a node's inputs.
    ///
    /// # Errors
    ///
    /// As [`MockDaemon::send_event`].
    pub fn close_input(
        &self,
        node: &NodeId,
        input: &DataId,
        reason: RouteCloseReason,
    ) -> Result<()> {
        let source = lock(&self.inner.nodes)
            .get(node)
            .and_then(|state| state.spec.input(input).map(|spec| spec.source.clone()))
            .unwrap_or_else(|| PortRef::new(node.clone(), input.clone()));
        self.send_event(
            node,
            NodeEvent::InputClosed {
                id: input.clone(),
                source,
                reason,
            },
        )
    }

    /// Stops one node.
    ///
    /// # Errors
    ///
    /// As [`MockDaemon::send_event`].
    pub fn stop(&self, node: &NodeId, cause: StopCause) -> Result<()> {
        self.send_event(node, NodeEvent::Stop { cause, grace: None })
    }

    /// Stops every connected node.
    pub fn stop_all(&self, cause: StopCause) {
        for node in self.node_ids() {
            let _ = self.stop(&node, cause.clone());
        }
    }

    /// Pushes a parameter update to a node (§17 `param set`).
    ///
    /// # Errors
    ///
    /// As [`MockDaemon::send_event`].
    pub fn set_param(&self, node: &NodeId, key: ParamKey, value: Parameter) -> Result<()> {
        self.send_event(
            node,
            NodeEvent::ParamUpdate {
                scope: ParamScope::node(self.inner.dataflow, node.clone()),
                key,
                value,
            },
        )
    }

    /// Offers a node a route upgrade (§6.3).
    ///
    /// # Errors
    ///
    /// As [`MockDaemon::send_event`].
    pub fn upgrade_route(
        &self,
        node: &NodeId,
        output: &DataId,
        segment: ShmSegmentSpec,
        consumers: Vec<PortRef>,
    ) -> Result<()> {
        self.send_event(
            node,
            NodeEvent::RouteUpgrade {
                output: output.clone(),
                segment,
                consumers,
            },
        )
    }

    /// Takes a node's route back to the daemon path (§6.3).
    ///
    /// # Errors
    ///
    /// As [`MockDaemon::send_event`].
    pub fn downgrade_route(
        &self,
        node: &NodeId,
        output: &DataId,
        reason: RouteDowngradeReason,
    ) -> Result<()> {
        self.send_event(
            node,
            NodeEvent::RouteDowngrade {
                output: output.clone(),
                reason,
            },
        )
    }

    // ---------------------------------------------------------- inspection

    /// Every request the daemon has received.
    #[must_use]
    pub fn requests(&self) -> Vec<RecordedRequest> {
        lock(&self.inner.requests).clone()
    }

    /// Every message the daemon has seen published.
    #[must_use]
    pub fn sends(&self) -> Vec<RecordedSend> {
        lock(&self.inner.sends).clone()
    }

    /// The messages one node published on one output.
    #[must_use]
    pub fn sends_on(&self, node: &NodeId, output: &DataId) -> Vec<RecordedSend> {
        lock(&self.inner.sends)
            .iter()
            .filter(|send| &send.node == node && &send.output == output)
            .cloned()
            .collect()
    }

    /// Every log record the daemon has received.
    #[must_use]
    pub fn logs(&self) -> Vec<LogRecord> {
        lock(&self.inner.logs).clone()
    }

    /// One entry of the extension table.
    #[must_use]
    pub fn extension(&self, key: &ExtensionKey) -> Option<Vec<u8>> {
        lock(&self.inner.extensions).get(&key.to_string()).cloned()
    }

    /// Whether a node has subscribed to its inputs.
    #[must_use]
    pub fn is_subscribed(&self, node: &NodeId) -> bool {
        lock(&self.inner.nodes)
            .get(node)
            .is_some_and(|state| state.subscribed)
    }

    /// Waits until `predicate` holds over the recorded requests.
    ///
    /// The synchronisation primitive a test needs when the node's own thread
    /// is doing the work: assert on *what the daemon saw*, not on a sleep.
    ///
    /// # Errors
    ///
    /// [`NodeError::Timeout`] when it never holds.
    pub fn wait_for(
        &self,
        timeout: Duration,
        mut predicate: impl FnMut(&[RecordedRequest]) -> bool,
    ) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            let ticket = self.inner.signal.ticket();
            if predicate(&lock(&self.inner.requests)) {
                return Ok(());
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(NodeError::Timeout {
                    operation: "MockDaemon::wait_for",
                    millis: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                });
            };
            if self.inner.signal.wait_blocking(ticket, Some(remaining)) == WaitOutcome::Closed {
                return Err(NodeError::Testing("the mock daemon stopped".to_owned()));
            }
        }
    }

    /// Waits until `node` has sent at least `count` messages on `output`.
    ///
    /// # Errors
    ///
    /// As [`MockDaemon::wait_for`].
    pub fn wait_for_sends(
        &self,
        node: &NodeId,
        output: &DataId,
        count: usize,
        timeout: Duration,
    ) -> Result<Vec<RecordedSend>> {
        let inner = Arc::clone(&self.inner);
        let wanted_node = node.clone();
        let wanted_output = output.clone();
        self.wait_for(timeout, move |_| {
            lock(&inner.sends)
                .iter()
                .filter(|send| send.node == wanted_node && send.output == wanted_output)
                .count()
                >= count
        })?;
        Ok(self.sends_on(node, output))
    }

    /// Listens on a real Unix-domain socket, the way a daemon actually does
    /// (§4.2).
    ///
    /// Every other entry point here hands a node one end of an in-process
    /// duplex; this one exercises the dial, the socket and the greeting a
    /// deployed node performs. Nodes that connect this way are admitted from
    /// their `Register` handshake, so a `path: dynamic` node (§8.3) can attach
    /// itself with nothing pre-arranged.
    ///
    /// # Errors
    ///
    /// [`NodeError::Testing`] when the socket cannot be bound.
    #[cfg(unix)]
    pub fn listen_unix(&self, path: impl AsRef<std::path::Path>) -> Result<UnixEndpoint> {
        let path = path.as_ref().to_path_buf();
        // A stale socket from a crashed run would make `bind` fail; removing
        // it is what a daemon does at start-up too.
        let _ = std::fs::remove_file(&path);
        let bound = path.clone();
        let listener = self
            .runtime
            .block_on(
                "MockDaemon::listen_unix",
                "MockDaemon::listen_unix",
                async move { tokio::net::UnixListener::bind(bound) },
            )?
            .map_err(|error| {
                NodeError::Testing(format!("cannot bind {}: {error}", path.display()))
            })?;

        let inner = Arc::clone(&self.inner);
        let task = self.runtime.spawn(async move {
            loop {
                let Ok((stream, _addr)) = listener.accept().await else {
                    break;
                };
                let session = SessionId::from_u128(u128::from(
                    inner.next_session.fetch_add(1, Ordering::Relaxed),
                ));
                let (sender, receiver) = mpsc::unbounded_channel();
                let served = Arc::clone(&inner);
                let _connection = tokio::spawn(async move {
                    serve(served, stream, None, sender, receiver, session).await;
                });
            }
        });
        Ok(UnixEndpoint { path, task })
    }

    /// Stops the daemon: every waiter wakes and every node's link ends.
    pub fn shutdown(&self) {
        lock(&self.inner.nodes).clear();
        self.inner.signal.close();
    }
}

/// A listening Unix-domain socket, unlinked when it is dropped.
#[cfg(unix)]
#[derive(Debug)]
pub struct UnixEndpoint {
    /// Where it listens.
    path: std::path::PathBuf,
    /// The accept loop.
    task: tokio::task::JoinHandle<()>,
}

#[cfg(unix)]
impl UnixEndpoint {
    /// The socket's path.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// The endpoint string a node dials.
    #[must_use]
    pub fn endpoint(&self) -> String {
        format!("uds://{}", self.path.display())
    }
}

#[cfg(unix)]
impl Drop for UnixEndpoint {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Serves one node's connection.
///
/// `node` is `Some` when the daemon already knows who is dialling — the
/// in-process path, where [`MockDaemon::connect_node`] registered the
/// specification first — and `None` for a connection that arrived on a
/// listening socket, where the node's identity is learned from its
/// `Register` (§7.3) exactly as a real daemon learns it.
async fn serve<S>(
    inner: Arc<DaemonInner>,
    io: S,
    node: Option<NodeId>,
    sender: mpsc::UnboundedSender<NodeEvent>,
    mut events: mpsc::UnboundedReceiver<NodeEvent>,
    session: SessionId,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    let mut duplex = wrap_stream(io, FrameLimits::uds());
    let acceptor = Acceptor::new(inner.auth.clone()).with_accepted_roles(RoleSet::NODES);
    if accept(
        &mut duplex,
        &acceptor,
        SessionAssignment::Fresh(session),
        HANDSHAKE_TIMEOUT,
    )
    .await
    .is_err()
    {
        return;
    }

    let mut current = node;
    let (mut reader, mut writer) = duplex.into_halves();
    loop {
        tokio::select! {
            frame = reader.recv_frame() => {
                let Ok(Some(frame)) = frame else { break };
                match frame.kind() {
                    FrameKind::NodeRequest => {
                        let Ok(request) = NodeRequest::from_frame(&frame.as_view()) else {
                            continue;
                        };
                        if let NodeRequest::Register(handshake) = &request {
                            current = Some(admit(&inner, handshake, &sender));
                        }
                        let Some(node) = current.clone() else {
                            // Nothing before `Register` can be attributed to a
                            // node, so there is nothing sensible to do with it.
                            continue;
                        };
                        if let Some(answer) = handle_request(&inner, &node, &request, session)
                            && writer.send_message(&answer).await.is_err()
                        {
                            break;
                        }
                    }
                    FrameKind::Log => {
                        if let Ok(log) =
                            astrs_wire::LogFrame::from_frame(&frame.as_view())
                        {
                            lock(&inner.logs).push(log.record);
                            inner.signal.notify();
                        }
                    }
                    _ => {}
                }
            }
            event = events.recv() => {
                let Some(event) = event else { break };
                if writer.send_message(&event).await.is_err() {
                    break;
                }
            }
        }
    }
    let _ = writer.shutdown().await;
    if let Some(node) = current {
        let _removed = lock(&inner.nodes).remove(&node);
    }
}

/// Admits a registering node, inventing a specification for one the daemon
/// has never heard of.
///
/// A `path: dynamic` node (§8.3) declares its own ports in the handshake and
/// is spawned by nobody, so this is the only place its wiring comes from.
fn admit(
    inner: &Arc<DaemonInner>,
    handshake: &astrs_wire::NodeHandshake,
    sender: &mpsc::UnboundedSender<NodeEvent>,
) -> NodeId {
    let node = handshake.node.clone();
    let mut nodes = lock(&inner.nodes);
    if nodes.contains_key(&node) {
        return node;
    }
    let mut spec = NodeSpawnSpec::new(
        handshake.dataflow,
        node.clone(),
        handshake.generation,
        astrs_wire::NodeSource::Dynamic,
    );
    for input in &handshake.inputs {
        // A dynamic node names its inputs but not their producers; the port
        // it reads from is the one the graph will wire, and until then it
        // reads from a port of its own name.
        spec = spec.with_input(astrs_wire::InputSpec::new(
            input.clone(),
            PortRef::new(node.clone(), input.clone()),
        ));
    }
    for output in &handshake.outputs {
        spec = spec.with_output(astrs_wire::OutputSpec::new(output.clone()));
    }
    let _previous = nodes.insert(
        node.clone(),
        NodeState {
            spec,
            events: sender.clone(),
            subscribed: false,
        },
    );
    node
}

/// Applies one request, returning the immediate answer when there is one.
fn handle_request(
    inner: &Arc<DaemonInner>,
    node: &NodeId,
    request: &NodeRequest,
    session: SessionId,
) -> Option<NodeEvent> {
    inner.record_request(node, request);
    match request {
        NodeRequest::Register(_) => {
            let spec = lock(&inner.nodes)
                .get(node)
                .map(|state| state.spec.clone())?;
            Some(NodeEvent::Registered {
                spec: Box::new(spec),
                session,
            })
        }
        NodeRequest::Subscribe { .. } => {
            if let Some(state) = lock(&inner.nodes).get_mut(node) {
                state.subscribed = true;
            }
            None
        }
        NodeRequest::SendMessage {
            output,
            metadata,
            payload,
        } => {
            lock(&inner.sends).push(RecordedSend {
                node: node.clone(),
                output: output.clone(),
                metadata: metadata.clone(),
                payload: payload.clone(),
            });
            inner.signal.notify();
            route(inner, node, output, metadata, payload);
            None
        }
        NodeRequest::OutputDone { output } => {
            close_route(inner, node, output);
            None
        }
        NodeRequest::CloseOutputs { outputs } => {
            let all: Vec<DataId> = if outputs.is_empty() {
                lock(&inner.nodes)
                    .get(node)
                    .map(|state| {
                        state
                            .spec
                            .outputs
                            .iter()
                            .map(|out| out.id.clone())
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                outputs.clone()
            };
            for output in &all {
                close_route(inner, node, output);
            }
            None
        }
        NodeRequest::ExtStore { key, value, .. } => {
            let _previous = lock(&inner.extensions).insert(key.to_string(), value.clone());
            None
        }
        NodeRequest::ExtLoad { key } => {
            let value = lock(&inner.extensions).get(&key.to_string()).cloned();
            Some(NodeEvent::ExtValue {
                key: key.clone(),
                value,
            })
        }
        NodeRequest::ExtDrop { key } => {
            let _removed = lock(&inner.extensions).remove(&key.to_string());
            None
        }
        // `NextEvent`, `EventStreamDropped` and `RouteUpgradeAck` are recorded
        // above; none of them has an answer.
        _ => None,
    }
}

/// Delivers one published message to every consumer of its port.
fn route(
    inner: &Arc<DaemonInner>,
    node: &NodeId,
    output: &DataId,
    metadata: &Metadata,
    payload: &OutputPayload,
) {
    let port = PortRef::new(node.clone(), output.clone());
    let bytes = payload.bytes().unwrap_or_default().to_vec();
    for (consumer, input) in inner.consumers_of(&port) {
        let _delivered = inner.deliver(
            &consumer,
            NodeEvent::Input {
                id: input,
                source: port.clone(),
                metadata: metadata.clone(),
                payload: bytes.clone(),
            },
        );
    }
}

/// Tells every consumer of a port that it has closed.
fn close_route(inner: &Arc<DaemonInner>, node: &NodeId, output: &DataId) {
    let port = PortRef::new(node.clone(), output.clone());
    for (consumer, input) in inner.consumers_of(&port) {
        let _delivered = inner.deliver(
            &consumer,
            NodeEvent::InputClosed {
                id: input,
                source: port.clone(),
                reason: RouteCloseReason::ProducerFinished,
            },
        );
    }
}

/// Locks a mutex, recovering from a poisoning panic elsewhere.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::Event;
    use astrs_wire::{InputSpec, NodeSource, OutputSpec};

    fn producer_spec(daemon: &MockDaemon) -> NodeSpawnSpec {
        NodeSpawnSpec::new(
            daemon.dataflow(),
            NodeId::new("camera").unwrap(),
            0,
            NodeSource::Dynamic,
        )
        .with_output(OutputSpec::new(DataId::new("image").unwrap()))
    }

    fn consumer_spec(daemon: &MockDaemon) -> NodeSpawnSpec {
        NodeSpawnSpec::new(
            daemon.dataflow(),
            NodeId::new("detect").unwrap(),
            0,
            NodeSource::Dynamic,
        )
        .with_input(InputSpec::new(
            DataId::new("frames").unwrap(),
            PortRef::from_parts("camera", "image").unwrap(),
        ))
    }

    #[test]
    fn a_node_registers_and_subscribes() {
        let daemon = MockDaemon::start().unwrap();
        let (node, _events) = daemon.connect_node(consumer_spec(&daemon)).unwrap();
        let id = node.id().clone();

        daemon
            .wait_for(Duration::from_secs(5), |requests| {
                requests
                    .iter()
                    .any(|entry| matches!(entry.request, NodeRequest::Subscribe { .. }))
            })
            .unwrap();
        assert!(daemon.is_subscribed(&id));
        assert_eq!(daemon.node_ids(), vec![id]);

        let kinds: Vec<&str> = daemon
            .requests()
            .iter()
            .map(|entry| entry.request.variant_name())
            .collect();
        assert_eq!(kinds, vec!["Register", "Subscribe"]);
        assert!(format!("{daemon:?}").contains("detect"));
    }

    #[test]
    fn one_nodes_output_becomes_anothers_input() {
        let daemon = MockDaemon::start().unwrap();
        let (mut producer, _pe) = daemon.connect_node(producer_spec(&daemon)).unwrap();
        let (_consumer, mut events) = daemon.connect_node(consumer_spec(&daemon)).unwrap();

        let mut image = producer.raw_output("image").unwrap();
        image
            .send_bytes(vec![1, 2, 3], producer.metadata())
            .unwrap();

        let event = events
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .expect("the routed input");
        let Some((id, _, data)) = event.into_input() else {
            panic!("expected an input");
        };
        assert_eq!(id.as_str(), "frames");
        assert_eq!(data.to_vec(), vec![1, 2, 3]);

        let sends = daemon.sends_on(
            &NodeId::new("camera").unwrap(),
            &DataId::new("image").unwrap(),
        );
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].bytes(), Some(&[1, 2, 3][..]));
        assert_eq!(sends[0].port().to_string(), "camera/image");
    }

    #[test]
    fn closing_an_output_closes_the_consumer_input() {
        let daemon = MockDaemon::start().unwrap();
        let (mut producer, _pe) = daemon.connect_node(producer_spec(&daemon)).unwrap();
        let (_consumer, mut events) = daemon.connect_node(consumer_spec(&daemon)).unwrap();

        let mut image = producer.raw_output("image").unwrap();
        image.close().unwrap();

        let event = events
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .expect("the close");
        assert!(matches!(event, Event::InputClosed { .. }));
    }

    #[test]
    fn the_daemon_can_drive_a_node_directly() {
        let daemon = MockDaemon::start().unwrap();
        let (node, mut events) = daemon.connect_node(consumer_spec(&daemon)).unwrap();
        let id = node.id().clone();
        daemon
            .send_input(
                &id,
                &DataId::new("frames").unwrap(),
                Metadata::default(),
                vec![9],
            )
            .unwrap();
        let event = events
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(event.payload().map(crate::Payload::len), Some(1));

        daemon.stop(&id, StopCause::Requested).unwrap();
        let event = events
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert!(event.is_stop());
        assert!(events.is_fused());
    }

    #[test]
    fn an_unknown_node_is_reported() {
        let daemon = MockDaemon::start().unwrap();
        let error = daemon
            .stop(&NodeId::new("ghost").unwrap(), StopCause::Requested)
            .unwrap_err();
        assert!(matches!(error, NodeError::Testing(_)), "{error}");
    }

    #[test]
    fn waiting_for_something_that_never_happens_times_out() {
        let daemon = MockDaemon::start().unwrap();
        let error = daemon
            .wait_for(Duration::from_millis(50), |_| false)
            .unwrap_err();
        assert!(matches!(error, NodeError::Timeout { .. }), "{error}");
    }

    #[test]
    fn the_documented_constants_are_stable() {
        assert_eq!(DUPLEX_BUFFER, 1024 * 1024);
        assert_eq!(HANDSHAKE_TIMEOUT, Duration::from_secs(5));
    }
}
