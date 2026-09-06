//! The bridge's lifecycle: build the endpoints, pump both sides, leave
//! cleanly.
//!
//! # One loop, two sources
//!
//! The AstRS side is an [`EventStream`] and the ROS 2 side is a set of DDS
//! subscriptions, and both are asynchronous. Rather than a thread per side
//! with a lock between them, every subscription forwards into one bounded
//! channel ([`crate::topic::spawn_pump`]) and the loop `select!`s that
//! channel against the event stream. So there is exactly one place that
//! touches an endpoint's mutable state, no lock anywhere, and the
//! correlation tables in [`crate::service`] and [`crate::action`] need no
//! synchronisation at all.
//!
//! # Leaving cleanly
//!
//! §12 asks for restart-friendly nodes, and a DDS participant that is not
//! shut down is exactly the thing that makes a restart worse than a start:
//! the peer keeps the stale endpoints matched until its liveliness lease
//! expires, so the first seconds after a restart deliver samples to a
//! reader that no longer exists. On [`Event::Stop`] — or
//! [`Event::AllInputsClosed`], or the stream ending — the bridge therefore,
//! in this order:
//!
//! 1. aborts every pump task, dropping its subscription handle;
//! 2. destroys every reader and writer it created, so SEDP announces their
//!    departure;
//! 3. shuts the participant down, so SPDP announces *its* departure;
//! 4. closes its AstRS outputs.
//!
//! Nothing here is best-effort-and-forget: each step is awaited, and the
//! whole sequence is bounded by [`SHUTDOWN_TIMEOUT`] so a wedged peer
//! cannot keep a restarting node from exiting.

use std::sync::Arc;
use std::time::Duration;

use astrs_manifest::{BridgeDirection, Ros2Config, Ros2Role};
use astrs_node_api::{Event, EventStream, Node};
use astrs_ros2::node::{ContextOptions, NodeOptions, Ros2Context, Ros2Node};
use astrs_rtps::structure::Locator;
use tokio::sync::mpsc;

use crate::action::ActionEndpointSet;
use crate::config::{BridgeSettings, bridge_config};
use crate::error::{BridgeError, BridgeResult};
use crate::plan::{BridgePlan, plan};
use crate::resolve::{ResolvedPlan, resolve_plan};
use crate::service::ServiceEndpoint;
use crate::topic::{
    EndpointSlot, InboundSample, InboundTopic, OutboundTopic, Pumps, Subscriptions,
};

/// Worker threads the bridge's own runtime starts with.
///
/// Two, matching [`astrs_node_api::runtime::OWNED_WORKER_THREADS`]: the
/// pumps and the participant's cadence are both I/O-bound, and a bridge's
/// work belongs on the graph's nodes rather than in a thread pool here.
pub const WORKER_THREADS: usize = 2;

/// How many samples may be queued between the pumps and the loop.
///
/// The channel is the bridge's only queue, and a bounded one is what turns
/// a slow consumer into backpressure on the RTPS reader rather than into
/// unbounded memory. Two hundred and fifty-six is roughly twenty-five
/// KEEP_LAST(10) topics' worth of burst.
pub const CHANNEL_CAPACITY: usize = 256;

/// The whole shutdown sequence's budget.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Join the dataflow, read the `ros2:` block, and bridge until told to stop.
///
/// # Errors
///
/// Any [`BridgeError`]: the handshake, the block, the plan, a type that
/// cannot be resolved, or a participant that will not start.
pub fn run() -> BridgeResult<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKER_THREADS)
        .enable_all()
        .build()
        .map_err(|error| BridgeError::Runtime(error.to_string()))?;

    runtime.block_on(async {
        let (node, events) = Node::init_from_env()?;
        let config = bridge_config(node.descriptor())?;
        let settings = BridgeSettings::from_spec(node.descriptor())?;
        run_with(node, events, &config, &settings).await
    })
}

/// Bridge an already-connected node.
///
/// The seam a test drives: a [`astrs_node_api::testing::MockDaemon`] hands
/// back a `(Node, EventStream)` pair exactly like the real handshake does,
/// so everything below this line is exercised without a daemon.
///
/// # Errors
///
/// As [`run`], minus the handshake.
pub async fn run_with(
    node: Node,
    events: EventStream,
    config: &Ros2Config,
    settings: &BridgeSettings,
) -> BridgeResult<()> {
    let plan = plan(config, node.descriptor())?;
    let resolved = resolve_plan(&plan, settings)?;
    let bridge = Bridge::start(node, events, resolved, settings).await?;
    bridge.pump().await
}

/// Everything one running bridge owns.
#[derive(Debug)]
pub struct Bridge {
    node: Node,
    events: EventStream,
    context: Arc<Ros2Context>,
    /// Held so the ROS graph entry (and therefore `ros2 node list`) lives
    /// as long as the bridge does.
    ros: Ros2Node,
    inbound: Vec<InboundTopic>,
    outbound: Vec<OutboundTopic>,
    services: Vec<ServiceEndpoint>,
    actions: Vec<ActionEndpointSet>,
    subscriptions: Subscriptions,
    pumps: Pumps,
    samples: mpsc::Receiver<InboundSample>,
}

impl Bridge {
    /// Create every endpoint the plan calls for and start pumping the ROS
    /// side into a channel.
    ///
    /// # Errors
    ///
    /// [`BridgeError::Ros2`] when the participant or an endpoint will not
    /// start, and [`BridgeError::Node`] when a declared port is missing.
    pub async fn start(
        mut node: Node,
        events: EventStream,
        resolved: ResolvedPlan,
        settings: &BridgeSettings,
    ) -> BridgeResult<Self> {
        let context = Ros2Context::new(context_options(&resolved.plan, settings)).await?;
        let ros = Ros2Node::new(
            Arc::clone(&context),
            resolved.plan.node_name.clone(),
            node_options(&resolved.plan),
        )
        .await?;

        let (sender, samples) = mpsc::channel(CHANNEL_CAPACITY);
        let mut inbound = Vec::new();
        let mut outbound = Vec::new();
        let mut services = Vec::new();
        let mut actions = Vec::new();
        let mut subscriptions = Subscriptions::new();
        let mut pumps = Pumps::new();

        for topic in &resolved.plan.topics {
            let binding = binding_of(&resolved, &topic.message_type)?;
            match topic.direction {
                BridgeDirection::ToAstrs => {
                    let index = inbound.len();
                    let (endpoint, subscription) =
                        crate::topic::open_inbound(&mut node, &ros, topic.clone(), binding).await?;
                    let subscription = Arc::new(subscription);
                    pumps.push(crate::topic::spawn_pump(
                        EndpointSlot::Topic(index),
                        Arc::clone(&subscription),
                        sender.clone(),
                    ));
                    subscriptions.push(subscription);
                    inbound.push(endpoint);
                }
                BridgeDirection::FromAstrs => {
                    outbound.push(crate::topic::open_outbound(&ros, topic.clone(), binding).await?);
                }
            }
        }

        for service in &resolved.plan.services {
            let index = services.len();
            let (endpoint, subscription) =
                crate::service::open(&mut node, &ros, &resolved, service.clone()).await?;
            let slot = match service.role {
                Ros2Role::Server => EndpointSlot::ServiceRequest(index),
                Ros2Role::Client => EndpointSlot::ServiceReply(index),
            };
            pumps.push(crate::topic::spawn_pump(
                slot,
                Arc::clone(&subscription),
                sender.clone(),
            ));
            subscriptions.push(subscription);
            services.push(endpoint);
        }

        for action in &resolved.plan.actions {
            let index = actions.len();
            let (endpoint, action_subscriptions) =
                crate::action::open(&mut node, &ros, &resolved, action.clone(), index).await?;
            for (slot, subscription) in action_subscriptions {
                pumps.push(crate::topic::spawn_pump(
                    slot,
                    Arc::clone(&subscription),
                    sender.clone(),
                ));
                subscriptions.push(subscription);
            }
            actions.push(endpoint);
        }

        drop(sender);

        tracing::info!(
            node = %ros.fully_qualified_name(),
            topics = resolved.plan.topics.len(),
            services = resolved.plan.services.len(),
            actions = resolved.plan.actions.len(),
            columnar = resolved.columnar_count(),
            opaque = resolved.bindings.len().saturating_sub(resolved.columnar_count()),
            multicast = ?context.multicast(),
            "ros 2 bridge ready"
        );

        Ok(Self {
            node,
            events,
            context,
            ros,
            inbound,
            outbound,
            services,
            actions,
            subscriptions,
            pumps,
            samples,
        })
    }

    /// The ROS 2 node this bridge presents on the graph.
    #[must_use]
    pub const fn ros_node(&self) -> &Ros2Node {
        &self.ros
    }

    /// The AstRS node this bridge runs as.
    #[must_use]
    pub const fn node(&self) -> &Node {
        &self.node
    }

    /// How many endpoints of each kind were created.
    #[must_use]
    pub fn endpoint_counts(&self) -> (usize, usize, usize, usize) {
        (
            self.inbound.len(),
            self.outbound.len(),
            self.services.len(),
            self.actions.len(),
        )
    }

    /// Pump both sides until the graph says stop, then shut down cleanly.
    ///
    /// # Errors
    ///
    /// [`BridgeError::Node`] when an AstRS output fails, and
    /// [`BridgeError::Ros2`] when a DDS write fails. A message that will
    /// not *convert* is never fatal — it is logged and dropped, so one bad
    /// peer cannot take the bridge down (§12).
    pub async fn pump(mut self) -> BridgeResult<()> {
        loop {
            tokio::select! {
                event = self.events.recv_async() => {
                    match event {
                        Some(event) => {
                            if self.handle_event(event).await? {
                                break;
                            }
                        }
                        // The daemon connection closed: shut down as if
                        // Stop had arrived, so the participant still leaves
                        // the domain properly.
                        None => break,
                    }
                }
                sample = self.samples.recv() => {
                    match sample {
                        Some(sample) => self.handle_sample(&sample).await?,
                        // Every pump is gone, which only happens once the
                        // pumps have been aborted.
                        None => break,
                    }
                }
            }
        }

        self.shutdown().await
    }

    /// Handle one AstRS event. Returns `true` when the bridge should stop.
    async fn handle_event(&mut self, event: Event) -> BridgeResult<bool> {
        match event {
            Event::Input { id, meta, data } => {
                if let Some(topic) = self.outbound.iter().find(|topic| topic.bridge.port == id) {
                    crate::topic::forward_outbound(topic, &meta, &data).await?;
                    return Ok(false);
                }
                if let Some(endpoint) = self
                    .services
                    .iter_mut()
                    .find(|endpoint| *endpoint.inbound_port() == id)
                {
                    crate::service::forward_outbound(endpoint, &meta, &data).await?;
                    return Ok(false);
                }
                if let Some(endpoint) = self
                    .actions
                    .iter_mut()
                    .find(|endpoint| *endpoint.primary_input() == id)
                {
                    crate::action::forward_primary(endpoint, &meta, &data).await?;
                    return Ok(false);
                }
                if let Some(endpoint) = self.actions.iter_mut().find(|endpoint| {
                    endpoint.bridge.role == Ros2Role::Server
                        && endpoint.bridge.feedback_port.as_ref() == Some(&id)
                }) {
                    crate::action::forward_feedback(endpoint, &data).await?;
                    return Ok(false);
                }
                tracing::debug!(input = %id, "an input reached the bridge with no endpoint behind it");
                Ok(false)
            }
            Event::Stop(cause) => {
                tracing::info!(?cause, "the ros 2 bridge was asked to stop");
                Ok(true)
            }
            Event::AllInputsClosed => {
                // Every input closing does *not* end an inbound-only
                // bridge: a `direction: to_astrs` node has no inputs at
                // all, so treating this as a stop would make it exit
                // before it had forwarded a single sample. It ends the
                // bridge only when nothing on the ROS side can produce
                // work either — that is exactly "no pumps".
                Ok(self.pumps.is_empty())
            }
            Event::Error(message) => {
                tracing::warn!(message, "the session reported a condition");
                Ok(false)
            }
            other => {
                tracing::trace!(?other, "ignoring an event a bridge has nothing to do with");
                Ok(false)
            }
        }
    }

    /// Handle one DDS sample.
    async fn handle_sample(&mut self, sample: &InboundSample) -> BridgeResult<()> {
        match sample.slot {
            EndpointSlot::Topic(index) => {
                if let Some(topic) = self.inbound.get_mut(index) {
                    crate::topic::forward_inbound(&self.node, topic, sample)?;
                }
            }
            EndpointSlot::ServiceRequest(index) => {
                if let Some(endpoint) = self.services.get_mut(index) {
                    crate::service::forward_request(
                        &self.node,
                        endpoint,
                        &sample.payload,
                        &sample.info,
                    )?;
                }
            }
            EndpointSlot::ServiceReply(index) => {
                if let Some(endpoint) = self.services.get_mut(index) {
                    crate::service::forward_reply(
                        &self.node,
                        endpoint,
                        &sample.payload,
                        &sample.info,
                    )?;
                }
            }
            EndpointSlot::ActionGoalRequest(index) => {
                if let Some(endpoint) = self.actions.get_mut(index) {
                    crate::action::handle_goal_request(
                        &self.node,
                        endpoint,
                        &sample.payload,
                        &sample.info,
                    )
                    .await?;
                }
            }
            EndpointSlot::ActionGoalReply(index) => {
                if let Some(endpoint) = self.actions.get_mut(index) {
                    crate::action::handle_goal_reply(endpoint, &sample.payload).await?;
                }
            }
            EndpointSlot::ActionCancelRequest(index) => {
                if let Some(endpoint) = self.actions.get_mut(index) {
                    crate::action::handle_cancel_request(endpoint, &sample.payload).await?;
                }
            }
            EndpointSlot::ActionResultRequest(index) => {
                if let Some(endpoint) = self.actions.get_mut(index) {
                    crate::action::handle_result_request(endpoint, &sample.payload).await?;
                }
            }
            EndpointSlot::ActionResultReply(index) => {
                if let Some(endpoint) = self.actions.get_mut(index) {
                    crate::action::handle_result_reply(
                        &self.node,
                        endpoint,
                        &sample.payload,
                        &sample.info,
                    )?;
                }
            }
            EndpointSlot::ActionFeedback(index) => {
                if let Some(endpoint) = self.actions.get_mut(index) {
                    crate::action::handle_feedback(
                        &self.node,
                        endpoint,
                        &sample.payload,
                        &sample.info,
                    )?;
                }
            }
            EndpointSlot::ActionCancelReply(_) | EndpointSlot::ActionStatus(_) => {
                // A client bridge's cancel replies and the status topic are
                // informational: the graph learns a goal's outcome from
                // `get_result`, which is the authoritative endpoint.
                tracing::trace!(slot = ?sample.slot, "an informational action sample arrived");
            }
        }
        Ok(())
    }

    /// Tear the ROS 2 side down, then the AstRS side.
    ///
    /// See the module docs for the order and why it matters.
    async fn shutdown(mut self) -> BridgeResult<()> {
        let teardown = async {
            self.pumps.shutdown().await;
            self.subscriptions.shutdown().await;
            for topic in &self.outbound {
                if let Err(error) = topic.publisher.destroy().await {
                    tracing::debug!(%error, "an outbound publisher was already gone");
                }
            }
            if let Err(error) = self.ros.shutdown().await {
                tracing::debug!(%error, "the ros 2 node was already shut down");
            }
            self.context.shutdown().await;
        };

        if tokio::time::timeout(SHUTDOWN_TIMEOUT, teardown)
            .await
            .is_err()
        {
            tracing::warn!(
                "the ros 2 side did not shut down within {SHUTDOWN_TIMEOUT:?}; exiting anyway"
            );
        }

        self.node.close_outputs()?;
        Ok(())
    }
}

/// The participant options a plan and its settings call for.
#[must_use]
pub fn context_options(plan: &BridgePlan, settings: &BridgeSettings) -> ContextOptions {
    let mut options = ContextOptions::new(settings.domain_id)
        .with_compat(plan.compat)
        .with_multicast(settings.multicast)
        .with_graph_announcement(settings.announce_graph);
    options.bind = settings.bind;
    for peer in &settings.initial_peers {
        match peer.parse::<std::net::SocketAddr>() {
            Ok(address) => options = options.with_peer(Locator::from_socket_addr(address)),
            Err(error) => {
                tracing::warn!(peer, %error, "ignoring an unparseable initial peer");
            }
        }
    }
    options
}

/// The ROS node options a plan calls for.
///
/// The six parameter services are **off**: a bridge declares no parameters
/// of its own (its configuration is the manifest's, which is not writable at
/// runtime), and six services plus a `/parameter_events` publisher is six
/// endpoints of discovery traffic per bridge for something no caller can
/// usefully set. The node still appears in `ros2 node list` — that is the
/// `ros_discovery_info` announcement, which stays on.
#[must_use]
pub fn node_options(plan: &BridgePlan) -> NodeOptions {
    NodeOptions::in_namespace(plan.namespace.clone()).with_parameter_services(false)
}

/// The binding a resolved plan holds for a message type.
fn binding_of(
    resolved: &ResolvedPlan,
    ros_type_name: &str,
) -> BridgeResult<crate::resolve::TypeBinding> {
    resolved.binding(ros_type_name).cloned().ok_or_else(|| {
        BridgeError::Resolve(crate::error::ResolveError::UnknownType {
            kind: "message",
            type_name: ros_type_name.to_owned(),
            search: "it was not bound while the plan was resolved".to_owned(),
        })
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_manifest::RosCompat;
    use astrs_rtps::behavior::BindPolicy;

    use super::*;

    fn a_plan() -> BridgePlan {
        BridgePlan {
            compat: astrs_rtps::discovery::RosCompat::Humble,
            node_name: "lidar_in".to_owned(),
            namespace: "/robot1".to_owned(),
            topics: Vec::new(),
            services: Vec::new(),
            actions: Vec::new(),
        }
    }

    #[test]
    fn the_context_takes_its_domain_and_compat_from_the_plan_and_settings() {
        let settings = BridgeSettings {
            domain_id: 7,
            multicast: false,
            ..BridgeSettings::default()
        };
        let options = context_options(&a_plan(), &settings);
        assert_eq!(options.domain_id, 7);
        assert_eq!(options.compat, astrs_rtps::discovery::RosCompat::Humble);
        assert!(!options.multicast);
        assert!(options.announce_graph);
        assert_eq!(options.bind, BindPolicy::default());
    }

    #[test]
    fn an_initial_peer_becomes_a_locator_and_a_bad_one_is_skipped() {
        let settings = BridgeSettings {
            initial_peers: vec!["127.0.0.1:7400".to_owned(), "not-an-address".to_owned()],
            ..BridgeSettings::default()
        };
        let options = context_options(&a_plan(), &settings);
        assert_eq!(
            options.initial_peers.len(),
            1,
            "one good peer, one skipped: {:?}",
            options.initial_peers
        );
    }

    #[test]
    fn the_bind_policy_reaches_the_participant() {
        let settings = BridgeSettings {
            bind: BindPolicy::Standard(std::net::Ipv4Addr::new(10, 0, 0, 7)),
            ..BridgeSettings::default()
        };
        assert_eq!(
            context_options(&a_plan(), &settings).bind,
            BindPolicy::Standard(std::net::Ipv4Addr::new(10, 0, 0, 7))
        );
    }

    #[test]
    fn the_graph_announcement_can_be_turned_off() {
        let settings = BridgeSettings {
            announce_graph: false,
            ..BridgeSettings::default()
        };
        assert!(!context_options(&a_plan(), &settings).announce_graph);
    }

    #[test]
    fn the_node_mounts_in_the_plans_namespace_with_no_parameter_services() {
        let options = node_options(&a_plan());
        assert_eq!(options.namespace, "/robot1");
        assert!(!options.start_parameter_services);
    }

    #[test]
    fn a_jazzy_plan_produces_a_jazzy_participant() {
        let mut plan = a_plan();
        plan.compat = astrs_rtps::discovery::RosCompat::Jazzy;
        assert_eq!(
            context_options(&plan, &BridgeSettings::default()).compat,
            astrs_rtps::discovery::RosCompat::Jazzy
        );
        // …and the manifest spelling is what produced it.
        assert_eq!(
            crate::plan::compat_of(&astrs_manifest::Ros2Config {
                compat: RosCompat::Jazzy,
                topic: None,
                message_type: None,
                direction: None,
                topics: Vec::new(),
                service: None,
                action: None,
                role: None,
                qos: None,
                namespace: None,
                node_name: None,
            }),
            astrs_rtps::discovery::RosCompat::Jazzy
        );
    }

    #[test]
    fn the_channel_and_shutdown_budgets_are_bounded() {
        assert_eq!(CHANNEL_CAPACITY, 256);
        assert_eq!(SHUTDOWN_TIMEOUT, Duration::from_secs(5));
        assert_eq!(WORKER_THREADS, 2);
    }
}
