//! [`Ros2Node`]: the rcl-level node every other type in this crate hangs
//! off.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration as StdDuration;

use tokio::sync::Mutex;

use crate::action::{ActionClient, ActionQos, ActionServer};
use crate::error::{NameKind, Ros2Error, Ros2Result};
use crate::graph::{GraphCache, GraphQuery};
use crate::names::{FullName, NodeName};
use crate::node::context::Ros2Context;
use crate::node::options::{ContextOptions, NodeOptions};
use crate::parameters::descriptor::ParameterDescriptor;
use crate::parameters::service::ParameterServices;
use crate::parameters::store::{ListResult, ParameterChange, ParameterStore};
use crate::parameters::value::ParameterValue;
use crate::pubsub::{Publisher, RawPublisher, RawSubscription, Subscription};
use crate::qos::QosProfile;
use crate::service::{ServiceClient, ServiceServer};
use crate::time::{Ros2Clock, RosTime};
use crate::types::{ActionType, MessageType, ServiceType};

/// A ROS 2 node: a name, a namespace, and the entities created under them.
///
/// # What a node is, and is not
///
/// A node is **not** a DDS participant. Many nodes share one participant —
/// that is what a component container is — and
/// [`ros_discovery_info`](crate::graph::GraphAnnouncer) exists precisely
/// because DDS discovery cannot tell them apart. So a node is a name, a
/// remapping table, a parameter store, and a set of endpoints registered
/// under its name in the participant's graph sample.
///
/// # What it does on construction
///
/// - Registers itself in the participant's `ros_discovery_info` sample, so
///   `ros2 node list` sees it.
/// - Starts the six parameter services and the `/parameter_events`
///   publisher, unless
///   [`NodeOptions::start_parameter_services`] is off.
/// - Declares `use_sim_time` as a real parameter, because that is what it
///   is: `ros2 param set /talker use_sim_time true` has to work.
///
/// # Example
///
/// ```no_run
/// # use std::time::Duration;
/// use astrs_ros2::msg::std_msgs;
/// use astrs_ros2::node::{ContextOptions, NodeOptions, Ros2Node};
/// use astrs_ros2::qos::QosProfile;
///
/// # async fn example() -> astrs_ros2::Ros2Result<()> {
/// let node = Ros2Node::standalone(
///     "talker",
///     NodeOptions::default(),
///     ContextOptions::default(),
/// )
/// .await?;
///
/// let publisher = node
///     .create_publisher::<std_msgs::String>("chatter", QosProfile::default())
///     .await?;
/// publisher
///     .publish(&std_msgs::String { data: "hello".to_owned() })
///     .await?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct Ros2Node {
    inner: Arc<NodeInner>,
}

#[derive(Debug)]
struct NodeInner {
    name: NodeName,
    context: Arc<Ros2Context>,
    options: NodeOptions,
    parameters: Arc<Mutex<ParameterStore>>,
    services: Mutex<Option<ParameterServices>>,
    graph: Arc<GraphCache>,
    shut_down: AtomicBool,
}

impl Ros2Node {
    /// Create a node on an existing context.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::InvalidNodeName`] or [`Ros2Error::InvalidNamespace`] for
    /// a malformed identity, and [`Ros2Error::Rtps`] when an endpoint cannot
    /// be created.
    pub async fn new(
        context: Arc<Ros2Context>,
        name: impl Into<String>,
        options: NodeOptions,
    ) -> Ros2Result<Self> {
        let name = NodeName::new(name, options.namespace.clone())?;
        let graph = GraphCache::new(Arc::clone(&context)).await?;

        let mut store = if options.allow_undeclared_parameters {
            ParameterStore::permissive()
        } else {
            ParameterStore::new()
        };
        // `use_sim_time` is a real parameter on every ROS 2 node, not a
        // constructor flag: `ros2 param set … use_sim_time true` must work.
        store.declare("use_sim_time", options.use_sim_time)?;
        context.clock().set_sim_time(options.use_sim_time);
        let parameters = Arc::new(Mutex::new(store));

        if let Some(announcer) = context.announcer() {
            announcer.add_node(name.namespace(), name.name()).await?;
        }

        let node = Self {
            inner: Arc::new(NodeInner {
                name: name.clone(),
                context: Arc::clone(&context),
                options: options.clone(),
                parameters: Arc::clone(&parameters),
                services: Mutex::new(None),
                graph,
                shut_down: AtomicBool::new(false),
            }),
        };

        if options.start_parameter_services {
            let services =
                ParameterServices::start(context, name, parameters, options.parameter_events_qos)
                    .await?;
            *node.inner.services.lock().await = Some(services);
        }

        Ok(node)
    }

    /// Create a node on a context of its own.
    ///
    /// The one-node-per-process shape, which is what most ROS 2 programs
    /// are.
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new), plus whatever
    /// [`Ros2Context::new`] reports.
    pub async fn standalone(
        name: impl Into<String>,
        options: NodeOptions,
        context: ContextOptions,
    ) -> Ros2Result<Self> {
        let context = Ros2Context::new(context).await?;
        Self::new(context, name, options).await
    }

    /// The node's name, without its namespace.
    #[must_use]
    pub fn name(&self) -> &str {
        self.inner.name.name()
    }

    /// The node's namespace.
    #[must_use]
    pub fn namespace(&self) -> &str {
        self.inner.name.namespace()
    }

    /// The node's fully-qualified name: what `ros2 node list` prints.
    #[must_use]
    pub fn fully_qualified_name(&self) -> String {
        self.inner.name.fully_qualified()
    }

    /// The node's checked identity.
    #[must_use]
    pub fn node_name(&self) -> &NodeName {
        &self.inner.name
    }

    /// The context this node lives on.
    #[must_use]
    pub fn context(&self) -> &Arc<Ros2Context> {
        &self.inner.context
    }

    /// The clock this node reads.
    #[must_use]
    pub fn clock(&self) -> &Ros2Clock {
        self.inner.context.clock()
    }

    /// The ROS graph as this node sees it.
    #[must_use]
    pub fn graph(&self) -> &Arc<GraphCache> {
        &self.inner.graph
    }

    /// The current ROS time.
    #[must_use]
    pub fn now(&self) -> RosTime {
        self.clock().now()
    }

    /// Expand and remap a topic name against this node.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::InvalidTopicName`] or [`Ros2Error::NameTooLong`].
    pub fn resolve_topic(&self, name: &str) -> Ros2Result<FullName> {
        self.inner
            .name
            .resolve_with(name, &self.inner.options.remap, NameKind::TopicName)
    }

    /// Expand and remap a service name against this node.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::InvalidServiceName`] or [`Ros2Error::NameTooLong`].
    pub fn resolve_service(&self, name: &str) -> Ros2Result<FullName> {
        self.inner
            .name
            .resolve_with(name, &self.inner.options.remap, NameKind::ServiceName)
    }

    /// Expand and remap an action name against this node.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::InvalidServiceName`] or [`Ros2Error::NameTooLong`].
    pub fn resolve_action(&self, name: &str) -> Ros2Result<FullName> {
        self.inner
            .name
            .resolve_with(name, &self.inner.options.remap, NameKind::ActionName)
    }

    /// Create a typed publication.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::NodeShutDown`], a naming error, or [`Ros2Error::Rtps`].
    pub async fn create_publisher<T: MessageType>(
        &self,
        topic: &str,
        qos: QosProfile,
    ) -> Ros2Result<Publisher<T>> {
        self.check_running()?;
        let name = self.resolve_topic(topic)?;
        Publisher::<T>::new(
            Arc::clone(&self.inner.context),
            name,
            qos,
            Some(self.fully_qualified_name()),
        )
        .await
    }

    /// Create a type-erased publication.
    ///
    /// # Errors
    ///
    /// As [`create_publisher`](Self::create_publisher).
    pub async fn create_raw_publisher(
        &self,
        topic: &str,
        dds_type: impl Into<String>,
        qos: QosProfile,
    ) -> Ros2Result<RawPublisher> {
        self.check_running()?;
        let name = self.resolve_topic(topic)?;
        RawPublisher::owned_by(
            Arc::clone(&self.inner.context),
            name,
            dds_type,
            qos,
            Some(self.fully_qualified_name()),
        )
        .await
    }

    /// Create a typed subscription.
    ///
    /// # Errors
    ///
    /// As [`create_publisher`](Self::create_publisher).
    pub async fn create_subscription<T: MessageType>(
        &self,
        topic: &str,
        qos: QosProfile,
    ) -> Ros2Result<Subscription<T>> {
        self.check_running()?;
        let name = self.resolve_topic(topic)?;
        Subscription::<T>::new(
            Arc::clone(&self.inner.context),
            name,
            qos,
            Some(self.fully_qualified_name()),
        )
        .await
    }

    /// Create a type-erased subscription.
    ///
    /// # Errors
    ///
    /// As [`create_publisher`](Self::create_publisher).
    pub async fn create_raw_subscription(
        &self,
        topic: &str,
        dds_type: impl Into<String>,
        qos: QosProfile,
    ) -> Ros2Result<RawSubscription> {
        self.check_running()?;
        let name = self.resolve_topic(topic)?;
        RawSubscription::owned_by(
            Arc::clone(&self.inner.context),
            name,
            dds_type,
            qos,
            Some(self.fully_qualified_name()),
        )
        .await
    }

    /// Create a service server.
    ///
    /// # Errors
    ///
    /// As [`create_publisher`](Self::create_publisher).
    pub async fn create_service<S: ServiceType>(
        &self,
        name: &str,
        qos: QosProfile,
    ) -> Ros2Result<ServiceServer<S>> {
        self.check_running()?;
        let resolved = self.resolve_service(name)?;
        ServiceServer::<S>::new(
            Arc::clone(&self.inner.context),
            resolved,
            qos,
            Some(self.fully_qualified_name()),
        )
        .await
    }

    /// Create a service client.
    ///
    /// # Errors
    ///
    /// As [`create_publisher`](Self::create_publisher).
    pub async fn create_client<S: ServiceType>(
        &self,
        name: &str,
        qos: QosProfile,
    ) -> Ros2Result<ServiceClient<S>> {
        self.check_running()?;
        let resolved = self.resolve_service(name)?;
        ServiceClient::<S>::new(
            Arc::clone(&self.inner.context),
            resolved,
            qos,
            Some(self.fully_qualified_name()),
        )
        .await
    }

    /// Create an action server.
    ///
    /// # Errors
    ///
    /// As [`create_publisher`](Self::create_publisher).
    pub async fn create_action_server<A: ActionType>(
        &self,
        name: &str,
        qos: ActionQos,
    ) -> Ros2Result<ActionServer<A>> {
        self.check_running()?;
        let resolved = self.resolve_action(name)?;
        ActionServer::<A>::new(
            Arc::clone(&self.inner.context),
            resolved,
            qos,
            Some(self.fully_qualified_name()),
        )
        .await
    }

    /// Create an action client.
    ///
    /// # Errors
    ///
    /// As [`create_publisher`](Self::create_publisher).
    pub async fn create_action_client<A: ActionType>(
        &self,
        name: &str,
        qos: ActionQos,
    ) -> Ros2Result<ActionClient<A>> {
        self.check_running()?;
        let resolved = self.resolve_action(name)?;
        ActionClient::<A>::new(
            Arc::clone(&self.inner.context),
            resolved,
            qos,
            Some(self.fully_qualified_name()),
        )
        .await
    }

    /// Declare a parameter, announcing it on `/parameter_events`.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::ParameterAlreadyDeclared`], a naming error, or a range
    /// or type violation.
    pub async fn declare_parameter(
        &self,
        name: &str,
        value: impl Into<ParameterValue>,
    ) -> Ros2Result<()> {
        let change = self.inner.parameters.lock().await.declare(name, value)?;
        self.announce(&[change]).await
    }

    /// Declare a parameter with an explicit descriptor.
    ///
    /// # Errors
    ///
    /// As [`declare_parameter`](Self::declare_parameter).
    pub async fn declare_parameter_with(
        &self,
        name: &str,
        value: impl Into<ParameterValue>,
        descriptor: ParameterDescriptor,
    ) -> Ros2Result<()> {
        let change = self
            .inner
            .parameters
            .lock()
            .await
            .declare_with(name, value, descriptor)?;
        self.announce(&[change]).await
    }

    /// Undeclare a parameter.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::UnknownParameter`] or [`Ros2Error::ParameterReadOnly`].
    pub async fn undeclare_parameter(&self, name: &str) -> Ros2Result<()> {
        let change = self.inner.parameters.lock().await.undeclare(name)?;
        self.announce(&[change]).await
    }

    /// Read a parameter.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::UnknownParameter`].
    pub async fn get_parameter(&self, name: &str) -> Ros2Result<ParameterValue> {
        self.inner.parameters.lock().await.get(name).cloned()
    }

    /// Read a parameter, or [`ParameterValue::NotSet`] if it is undeclared.
    pub async fn get_parameter_or_unset(&self, name: &str) -> ParameterValue {
        self.inner.parameters.lock().await.get_or_unset(name)
    }

    /// Set a parameter, announcing the change.
    ///
    /// Setting `use_sim_time` switches the node's clock, which is what makes
    /// `ros2 param set /talker use_sim_time true` do something.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::UnknownParameter`], [`Ros2Error::ParameterReadOnly`], or
    /// a type or range violation.
    pub async fn set_parameter(
        &self,
        name: &str,
        value: impl Into<ParameterValue>,
    ) -> Ros2Result<()> {
        let change = self.inner.parameters.lock().await.set(name, value)?;
        self.apply_side_effects(core::slice::from_ref(&change));
        self.announce(&[change]).await
    }

    /// Set several parameters as one transaction.
    ///
    /// # Errors
    ///
    /// The first error any update produces; none is applied.
    pub async fn set_parameters_atomically<I, N, V>(&self, updates: I) -> Ros2Result<()>
    where
        I: IntoIterator<Item = (N, V)>,
        N: AsRef<str>,
        V: Into<ParameterValue>,
    {
        let changes = self.inner.parameters.lock().await.set_atomically(updates)?;
        self.apply_side_effects(&changes);
        self.announce(&changes).await
    }

    /// List parameter names and prefixes.
    pub async fn list_parameters(&self, prefixes: &[String], depth: u64) -> ListResult {
        self.inner.parameters.lock().await.list(prefixes, depth)
    }

    /// Read a parameter's descriptor.
    pub async fn describe_parameter(&self, name: &str) -> ParameterDescriptor {
        self.inner.parameters.lock().await.describe(name)
    }

    /// True when a parameter is declared.
    pub async fn has_parameter(&self, name: &str) -> bool {
        self.inner.parameters.lock().await.has(name)
    }

    /// The parameter store, for a caller that needs more than the node's
    /// convenience methods.
    #[must_use]
    pub fn parameters(&self) -> &Arc<Mutex<ParameterStore>> {
        &self.inner.parameters
    }

    /// Whether this node's clock is following `/clock`.
    #[must_use]
    pub fn uses_sim_time(&self) -> bool {
        self.clock().uses_sim_time()
    }

    /// Every node on the graph: `ros2 node list`.
    pub async fn node_names(&self) -> Vec<String> {
        self.inner.graph.node_names(GraphQuery::new()).await
    }

    /// Every topic and the types on it: `ros2 topic list -t`.
    pub async fn topic_names_and_types(&self) -> std::collections::BTreeMap<String, Vec<String>> {
        self.inner
            .graph
            .topic_names_and_types(GraphQuery::new())
            .await
    }

    /// Every service and its type: `ros2 service list -t`.
    pub async fn service_names_and_types(&self) -> std::collections::BTreeMap<String, Vec<String>> {
        self.inner
            .graph
            .service_names_and_types(GraphQuery::new())
            .await
    }

    /// Every action and its type: `ros2 action list -t`.
    pub async fn action_names_and_types(&self) -> std::collections::BTreeMap<String, Vec<String>> {
        self.inner
            .graph
            .action_names_and_types(GraphQuery::new())
            .await
    }

    /// Wait until `node` appears on the graph.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Timeout`].
    pub async fn wait_for_node(&self, node: &str, timeout: StdDuration) -> Ros2Result<()> {
        self.inner.graph.wait_for_node(node, timeout).await
    }

    /// True once [`shutdown`](Self::shutdown) has run.
    #[must_use]
    pub fn is_shut_down(&self) -> bool {
        self.inner.shut_down.load(Ordering::Acquire)
    }

    /// Remove the node from the graph and stop its parameter services.
    ///
    /// Does **not** shut the context down: other nodes may still be on it.
    /// Idempotent.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Rtps`] or [`Ros2Error::Cdr`].
    pub async fn shutdown(&self) -> Ros2Result<()> {
        if self.inner.shut_down.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        if let Some(services) = self.inner.services.lock().await.take() {
            services.shutdown().await?;
        }
        self.inner.graph.shutdown().await?;
        if let Some(announcer) = self.inner.context.announcer() {
            announcer.remove_node(&self.fully_qualified_name()).await?;
        }
        Ok(())
    }

    /// Refuse an operation on a node that has been shut down.
    fn check_running(&self) -> Ros2Result<()> {
        if self.is_shut_down() {
            return Err(Ros2Error::NodeShutDown);
        }
        Ok(())
    }

    /// Apply the effects a parameter change has beyond the store.
    fn apply_side_effects(&self, changes: &[ParameterChange]) {
        for change in changes {
            if change.name() == "use_sim_time"
                && let Some(enabled) = change.value().as_bool()
            {
                self.clock().set_sim_time(enabled);
            }
        }
    }

    /// Publish a `/parameter_events` message, when the services are running.
    async fn announce(&self, changes: &[ParameterChange]) -> Ros2Result<()> {
        if let Some(services) = self.inner.services.lock().await.as_ref() {
            services.announce(changes).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::msg::std_msgs;

    async fn node(name: &str) -> Ros2Node {
        Ros2Node::standalone(name, NodeOptions::default(), ContextOptions::loopback())
            .await
            .expect("a node")
    }

    #[tokio::test]
    async fn a_node_has_an_identity_and_a_clock() {
        let node = node("talker").await;
        assert_eq!(node.name(), "talker");
        assert_eq!(node.namespace(), "/");
        assert_eq!(node.fully_qualified_name(), "/talker");
        assert!(node.now().is_positive());
        assert!(!node.is_shut_down());
        node.shutdown().await.expect("shutdown");
        node.context().shutdown().await;
    }

    #[tokio::test]
    async fn a_namespaced_node_qualifies_every_relative_name() {
        let node = Ros2Node::standalone(
            "talker",
            NodeOptions::in_namespace("/robot"),
            ContextOptions::loopback(),
        )
        .await
        .expect("a node");
        assert_eq!(node.fully_qualified_name(), "/robot/talker");
        assert_eq!(
            node.resolve_topic("scan").expect("valid").as_str(),
            "/robot/scan"
        );
        assert_eq!(
            node.resolve_topic("~/scan").expect("valid").as_str(),
            "/robot/talker/scan"
        );
        assert_eq!(
            node.resolve_topic("/absolute").expect("valid").as_str(),
            "/absolute"
        );
        node.shutdown().await.expect("shutdown");
        node.context().shutdown().await;
    }

    #[tokio::test]
    async fn a_bad_node_name_is_refused_before_anything_is_created() {
        let error = Ros2Node::standalone(
            "not a name",
            NodeOptions::default(),
            ContextOptions::loopback(),
        )
        .await
        .expect_err("malformed");
        assert!(matches!(error, Ros2Error::InvalidNodeName { .. }));
    }

    #[tokio::test]
    async fn a_node_registers_itself_in_the_graph_sample() {
        let node = node("talker").await;
        let announcer = node.context().announcer().expect("on");
        let snapshot = announcer.snapshot().await;
        assert!(snapshot.node("/talker").is_some());
        assert_eq!(node.node_names().await, vec!["/talker".to_owned()]);

        node.shutdown().await.expect("shutdown");
        assert!(
            announcer.snapshot().await.node("/talker").is_none(),
            "shutdown takes the node out of the graph"
        );
        node.context().shutdown().await;
    }

    #[tokio::test]
    async fn a_publisher_is_registered_under_its_node() {
        let node = node("talker").await;
        let publisher = node
            .create_publisher::<std_msgs::String>("chatter", QosProfile::default())
            .await
            .expect("publisher");
        assert_eq!(publisher.topic().as_str(), "/chatter");

        let announcer = node.context().announcer().expect("on");
        assert!(
            announcer
                .snapshot()
                .await
                .node("/talker")
                .expect("present")
                .owns(publisher.guid())
        );
        node.shutdown().await.expect("shutdown");
        node.context().shutdown().await;
    }

    #[tokio::test]
    async fn use_sim_time_is_a_real_parameter() {
        let node = node("talker").await;
        assert!(node.has_parameter("use_sim_time").await);
        assert_eq!(
            node.get_parameter("use_sim_time").await.expect("declared"),
            ParameterValue::Bool(false)
        );
        assert!(!node.uses_sim_time());

        node.set_parameter("use_sim_time", true)
            .await
            .expect("settable");
        assert!(
            node.uses_sim_time(),
            "setting the parameter switches the clock"
        );
        node.shutdown().await.expect("shutdown");
        node.context().shutdown().await;
    }

    #[tokio::test]
    async fn a_node_started_with_sim_time_has_it_on() {
        let node = Ros2Node::standalone(
            "talker",
            NodeOptions::default().with_sim_time(true),
            ContextOptions::loopback(),
        )
        .await
        .expect("a node");
        assert!(node.uses_sim_time());
        assert_eq!(
            node.get_parameter("use_sim_time").await.expect("declared"),
            ParameterValue::Bool(true)
        );
        node.shutdown().await.expect("shutdown");
        node.context().shutdown().await;
    }

    #[tokio::test]
    async fn parameters_declare_get_set_and_list() {
        let node = node("talker").await;
        node.declare_parameter("gain", 1.5_f64)
            .await
            .expect("declare");
        node.declare_parameter("camera.width", 640_i64)
            .await
            .expect("declare");

        assert_eq!(
            node.get_parameter("gain").await.expect("declared"),
            ParameterValue::Double(1.5)
        );
        node.set_parameter("gain", 2.5_f64).await.expect("set");
        assert_eq!(
            node.get_parameter("gain").await.expect("declared"),
            ParameterValue::Double(2.5)
        );

        let listed = node.list_parameters(&[], 0).await;
        assert!(listed.names.contains(&"gain".to_owned()));
        assert!(listed.names.contains(&"camera.width".to_owned()));
        assert!(listed.prefixes.contains(&"camera".to_owned()));

        assert_eq!(node.describe_parameter("gain").await.type_name(), "double");
        node.undeclare_parameter("gain").await.expect("undeclare");
        assert!(!node.has_parameter("gain").await);
        assert_eq!(
            node.get_parameter_or_unset("gain").await,
            ParameterValue::NotSet
        );

        node.shutdown().await.expect("shutdown");
        node.context().shutdown().await;
    }

    #[tokio::test]
    async fn an_atomic_set_applies_all_or_nothing_through_the_node() {
        let node = node("talker").await;
        node.declare_parameter("a", 1_i64).await.expect("declare");
        node.declare_parameter("b", 2_i64).await.expect("declare");

        assert!(
            node.set_parameters_atomically([("a", 10_i64), ("missing", 0_i64)])
                .await
                .is_err()
        );
        assert_eq!(
            node.get_parameter("a").await.expect("declared"),
            ParameterValue::Integer(1)
        );

        node.set_parameters_atomically([("a", 10_i64), ("b", 20_i64)])
            .await
            .expect("both");
        assert_eq!(
            node.get_parameter("b").await.expect("declared"),
            ParameterValue::Integer(20)
        );

        node.shutdown().await.expect("shutdown");
        node.context().shutdown().await;
    }

    #[tokio::test]
    async fn creating_anything_after_shutdown_is_refused() {
        let node = node("talker").await;
        node.shutdown().await.expect("shutdown");
        node.shutdown()
            .await
            .expect("shutting down twice is harmless");
        assert!(node.is_shut_down());

        let error = node
            .create_publisher::<std_msgs::String>("chatter", QosProfile::default())
            .await
            .expect_err("refused");
        assert_eq!(error, Ros2Error::NodeShutDown);
        node.context().shutdown().await;
    }

    #[tokio::test]
    async fn two_nodes_share_one_context_and_both_appear() {
        let context = Ros2Context::new(ContextOptions::loopback())
            .await
            .expect("bind");
        let first = Ros2Node::new(Arc::clone(&context), "talker", NodeOptions::default())
            .await
            .expect("a node");
        let second = Ros2Node::new(
            Arc::clone(&context),
            "listener",
            NodeOptions::in_namespace("/robot"),
        )
        .await
        .expect("a node");

        let mut names = first.node_names().await;
        names.sort();
        assert_eq!(
            names,
            vec!["/robot/listener".to_owned(), "/talker".to_owned()]
        );
        assert_eq!(second.context().guid(), first.context().guid());

        first.shutdown().await.expect("shutdown");
        assert_eq!(
            second.node_names().await,
            vec!["/robot/listener".to_owned()],
            "one node leaving does not take the other with it"
        );
        second.shutdown().await.expect("shutdown");
        context.shutdown().await;
    }

    #[tokio::test]
    async fn the_parameter_services_appear_on_the_graph() {
        let node = node("talker").await;
        let services = node.service_names_and_types().await;
        for leaf in crate::service::PARAMETER_SERVICE_NAMES {
            let expected = format!("/talker/{leaf}");
            assert!(
                services.contains_key(&expected),
                "{expected} is missing from {services:?}"
            );
        }
        node.shutdown().await.expect("shutdown");
        node.context().shutdown().await;
    }

    #[tokio::test]
    async fn parameter_services_can_be_turned_off() {
        let node = Ros2Node::standalone(
            "quiet",
            NodeOptions::default().with_parameter_services(false),
            ContextOptions::loopback(),
        )
        .await
        .expect("a node");
        assert!(
            node.service_names_and_types().await.is_empty(),
            "no parameter services means no services at all"
        );
        // The store still works; only the remote interface is absent.
        node.declare_parameter("gain", 1.0_f64)
            .await
            .expect("declare");
        node.shutdown().await.expect("shutdown");
        node.context().shutdown().await;
    }

    #[tokio::test]
    async fn a_remapped_topic_resolves_to_its_replacement() {
        let rules = crate::names::RemapRules::new()
            .with(crate::names::RemapRule::new("chatter", "/lidar/scan").expect("a valid rule"));
        let node = Ros2Node::standalone(
            "talker",
            NodeOptions::default().with_remap(rules),
            ContextOptions::loopback(),
        )
        .await
        .expect("a node");
        let publisher = node
            .create_publisher::<std_msgs::String>("chatter", QosProfile::default())
            .await
            .expect("publisher");
        assert_eq!(publisher.topic().as_str(), "/lidar/scan");
        assert_eq!(publisher.raw().dds_topic(), "rt/lidar/scan");
        node.shutdown().await.expect("shutdown");
        node.context().shutdown().await;
    }

    #[tokio::test]
    async fn a_permissive_node_declares_on_first_set() {
        let node = Ros2Node::standalone(
            "talker",
            NodeOptions::default().with_undeclared_parameters(true),
            ContextOptions::loopback(),
        )
        .await
        .expect("a node");
        node.set_parameter("undeclared", 1_i64)
            .await
            .expect("declared on demand");
        assert!(node.has_parameter("undeclared").await);
        node.shutdown().await.expect("shutdown");
        node.context().shutdown().await;
    }
    #[tokio::test]
    async fn dbg_local_endpoints() {
        let node = crate::node::Ros2Node::standalone(
            "talker",
            crate::node::NodeOptions::default(),
            crate::node::ContextOptions::loopback(),
        )
        .await
        .expect("a node");
        let local = node.context().local_endpoints().await;
        for (guid, endpoint) in &local {
            println!("LOCAL {guid} {} {}", endpoint.dds_topic, endpoint.is_writer);
        }
        println!("count={}", local.len());
        let endpoints = node
            .graph()
            .endpoints(crate::graph::GraphQuery::new().with_hidden())
            .await;
        for e in &endpoints {
            println!("EP {} {:?} {}", e.name, e.kind, e.node);
        }
    }
}
