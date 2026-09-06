//! [`GraphCache`]: what `ros2 node list`, `ros2 topic list -t` and
//! `ros2 service list` read.
//!
//! # Two sources, joined on a GID
//!
//! Neither half of ROS 2 discovery is enough on its own:
//!
//! | Source | Knows | Does not know |
//! |---|---|---|
//! | RTPS SEDP ([`DiscoveryDb`](astrs_rtps::discovery::DiscoveryDb)) | every endpoint's topic, type and QoS | which *node* owns it |
//! | `ros_discovery_info` ([`ParticipantEntitiesInfo`]) | which node owns which endpoint GID | nothing about the topic |
//!
//! A `ros2 topic info --verbose` line — "publisher `/talker` on
//! `/chatter`, type `std_msgs/msg/String`, RELIABLE, VOLATILE" — needs both,
//! joined on the endpoint's GID. That join is what this module is.
//!
//! # And a third: this participant itself
//!
//! RTPS discovery holds only *remote* endpoints; a participant does not
//! announce to itself. A cache built from the discovery database alone
//! would therefore show every node in the system except the one asking. So
//! [`Ros2Context`] records the endpoints it creates
//! ([`LocalEndpoint`](crate::node::LocalEndpoint)) and the cache merges
//! them, using the local [`GraphAnnouncer`](crate::graph::GraphAnnouncer)'s
//! own sample for the node names.
//!
//! # Hidden names
//!
//! `ros2 topic list` omits topics with an underscore-prefixed token unless
//! asked. Every listing method here takes the same choice through
//! [`GraphQuery`], so a caller decides once rather than filtering
//! afterwards.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration as StdDuration;

use astrs_rtps::discovery::{Gid, RosCompat};
use astrs_rtps::structure::Guid;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::error::Ros2Result;
use crate::graph::wire::ParticipantEntitiesInfo;
use crate::names::mangle::{
    ActionEndpoint, GRAPH_TOPIC, GRAPH_TYPE, TopicKind, demangle_type_name, ros_topic_name,
    split_action_name,
};
use crate::names::validate::is_hidden;
use crate::node::Ros2Context;
use crate::pubsub::RawSubscription;
use crate::qos::QosProfile;

/// One endpoint, as a ROS graph query sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointInfo {
    /// The node that owns it, fully qualified — empty when no
    /// `ros_discovery_info` sample claims it.
    pub node: String,
    /// The fully-qualified ROS topic or service name.
    pub name: String,
    /// The ROS type name, `pkg/msg/Type`.
    pub type_name: String,
    /// The endpoint's GID.
    pub gid: Gid,
    /// True for a publication, false for a subscription.
    pub is_publisher: bool,
    /// Whether the endpoint belongs to this participant.
    pub is_local: bool,
    /// Which prefix its DDS name carried.
    pub kind: TopicKind,
}

impl EndpointInfo {
    /// True when the endpoint has no node claiming it.
    ///
    /// Normal for a few milliseconds after a peer's SEDP arrives and before
    /// its `ros_discovery_info` does, and permanent for a non-ROS DDS
    /// participant on the same domain.
    #[must_use]
    pub fn is_unclaimed(&self) -> bool {
        self.node.is_empty()
    }
}

/// What a listing should include.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GraphQuery {
    /// Include names with an underscore-prefixed token.
    pub include_hidden: bool,
    /// Include this participant's own endpoints.
    ///
    /// On by default: a node that could not see itself in
    /// `ros2 topic list` would be a surprise.
    pub include_local: bool,
}

impl GraphQuery {
    /// The default: local endpoints, no hidden names.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            include_hidden: false,
            include_local: true,
        }
    }

    /// Include hidden names.
    #[must_use]
    pub const fn with_hidden(mut self) -> Self {
        self.include_hidden = true;
        self
    }

    /// Exclude this participant's own endpoints.
    #[must_use]
    pub const fn remote_only(mut self) -> Self {
        self.include_local = false;
        self
    }

    /// True when `name` passes the hidden filter.
    #[must_use]
    pub fn accepts(&self, name: &str) -> bool {
        self.include_hidden || !is_hidden(name)
    }
}

/// One node on the ROS graph.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct NodeInfo {
    /// The node's name, without its namespace.
    pub name: String,
    /// The node's namespace.
    pub namespace: String,
    /// The participant hosting it.
    pub participant: Guid,
    /// Whether it lives in this process's participant.
    pub is_local: bool,
    /// The `enclave=…;` the participant announced, when it announced one.
    pub enclave: Option<String>,
}

impl NodeInfo {
    /// The namespace and name joined: what `ros2 node list` prints.
    #[must_use]
    pub fn fully_qualified(&self) -> String {
        crate::names::expand::join(&self.namespace, &self.name)
    }

    /// True when `ros2 node list` would hide it.
    #[must_use]
    pub fn is_hidden(&self) -> bool {
        is_hidden(&self.fully_qualified())
    }
}

/// The ROS graph, kept up to date by a background task.
#[derive(Debug)]
pub struct GraphCache {
    context: Arc<Ros2Context>,
    subscription: RawSubscription,
    remote: Arc<Mutex<BTreeMap<Guid, ParticipantEntitiesInfo>>>,
    task: Mutex<Option<JoinHandle<()>>>,
    compat: RosCompat,
}

impl GraphCache {
    /// Subscribe to `ros_discovery_info` and start folding samples in.
    ///
    /// # Errors
    ///
    /// [`crate::Ros2Error::Rtps`] when the subscription cannot be created.
    pub async fn new(context: Arc<Ros2Context>) -> Ros2Result<Arc<Self>> {
        let compat = context.compat();
        let subscription = RawSubscription::new(
            Arc::clone(&context),
            crate::names::FullName::topic(format!("/{GRAPH_TOPIC}"))?,
            GRAPH_TYPE,
            QosProfile::graph(),
        )
        .await?;

        let cache = Arc::new(Self {
            context,
            subscription,
            remote: Arc::new(Mutex::new(BTreeMap::new())),
            task: Mutex::new(None),
            compat,
        });

        let pump = Arc::clone(&cache);
        let handle = tokio::spawn(async move { pump.pump().await });
        *cache.task.lock().await = Some(handle);
        Ok(cache)
    }

    /// How many remote participants have announced a graph sample.
    pub async fn known_participants(&self) -> usize {
        self.remote.lock().await.len()
    }

    /// Every node on the graph, sorted.
    pub async fn nodes(&self, query: GraphQuery) -> Vec<NodeInfo> {
        let mut nodes = Vec::new();
        let discovery = self.context.discovery().await;

        for (participant, info) in self.remote.lock().await.iter() {
            let enclave = discovery
                .participant(*participant)
                .and_then(|remote| parse_enclave(&remote.data.user_data));
            for node in &info.nodes {
                nodes.push(NodeInfo {
                    name: node.node_name.clone(),
                    namespace: node.node_namespace.clone(),
                    participant: *participant,
                    is_local: false,
                    enclave: enclave.clone(),
                });
            }
        }

        if query.include_local
            && let Some(announcer) = self.context.announcer()
        {
            let local = announcer.snapshot().await;
            for node in &local.nodes {
                nodes.push(NodeInfo {
                    name: node.node_name.clone(),
                    namespace: node.node_namespace.clone(),
                    participant: self.context.guid(),
                    is_local: true,
                    enclave: None,
                });
            }
        }

        nodes.retain(|node| query.accepts(&node.fully_qualified()));
        nodes.sort();
        nodes.dedup();
        nodes
    }

    /// Every node's fully-qualified name: what `ros2 node list` prints.
    pub async fn node_names(&self, query: GraphQuery) -> Vec<String> {
        let mut names: Vec<String> = self
            .nodes(query)
            .await
            .iter()
            .map(NodeInfo::fully_qualified)
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Every endpoint on the graph, remote and local.
    pub async fn endpoints(&self, query: GraphQuery) -> Vec<EndpointInfo> {
        let mut endpoints = Vec::new();
        let owners = self.owner_index(query).await;

        let discovery = self.context.discovery().await;
        for writer in discovery.writers() {
            if let Some(info) = describe(
                writer.guid(),
                &writer.identity.topic_name,
                &writer.identity.type_name,
                true,
                false,
                self.compat,
                &owners,
            ) {
                endpoints.push(info);
            }
        }
        for reader in discovery.readers() {
            if let Some(info) = describe(
                reader.guid(),
                &reader.identity.topic_name,
                &reader.identity.type_name,
                false,
                false,
                self.compat,
                &owners,
            ) {
                endpoints.push(info);
            }
        }

        if query.include_local {
            for (guid, local) in self.context.local_endpoints().await {
                if let Some(info) = describe(
                    guid,
                    &local.dds_topic,
                    &local.dds_type,
                    local.is_writer,
                    true,
                    self.compat,
                    &owners,
                ) {
                    endpoints.push(info);
                }
            }
        }

        endpoints.retain(|endpoint| query.accepts(&endpoint.name));
        endpoints.sort_by(|left, right| {
            (&left.name, &left.node, left.is_publisher).cmp(&(
                &right.name,
                &right.node,
                right.is_publisher,
            ))
        });
        endpoints
    }

    /// Topic names and the types carried on them: `ros2 topic list -t`.
    pub async fn topic_names_and_types(&self, query: GraphQuery) -> BTreeMap<String, Vec<String>> {
        self.names_and_types(query, |kind| kind == TopicKind::Topic)
            .await
    }

    /// Service names and their types: `ros2 service list -t`.
    ///
    /// A service appears once, under the ROS name both its halves share.
    /// The `_action/` endpoints are excluded: they belong to an action, and
    /// `ros2 service list` shows them only with `--include-hidden-services`.
    pub async fn service_names_and_types(
        &self,
        query: GraphQuery,
    ) -> BTreeMap<String, Vec<String>> {
        let mut found = self.names_and_types(query, TopicKind::is_service).await;
        if !query.include_hidden {
            found.retain(|name, _| split_action_name(name).is_none());
        }
        found
    }

    /// Action names and their types: `ros2 action list -t`.
    ///
    /// Derived from the five endpoints rather than announced: an action
    /// exists when its `send_goal` service does, and its type is that
    /// service's, with the `_SendGoal_Request` suffix removed.
    pub async fn action_names_and_types(&self, query: GraphQuery) -> BTreeMap<String, Vec<String>> {
        let mut actions: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for endpoint in self.endpoints(query.with_hidden()).await {
            let Some((action, ActionEndpoint::SendGoal)) = split_action_name(&endpoint.name) else {
                continue;
            };
            if !query.accepts(action) {
                continue;
            }
            if let Some(type_name) = action_type_of(&endpoint.type_name) {
                actions
                    .entry(action.to_owned())
                    .or_default()
                    .insert(type_name);
            }
        }
        actions
            .into_iter()
            .map(|(name, types)| (name, types.into_iter().collect()))
            .collect()
    }

    /// Every publication on `topic`.
    pub async fn publishers_of(&self, topic: &str, query: GraphQuery) -> Vec<EndpointInfo> {
        self.endpoints(query)
            .await
            .into_iter()
            .filter(|endpoint| {
                endpoint.is_publisher && endpoint.kind == TopicKind::Topic && endpoint.name == topic
            })
            .collect()
    }

    /// Every subscription on `topic`.
    pub async fn subscribers_of(&self, topic: &str, query: GraphQuery) -> Vec<EndpointInfo> {
        self.endpoints(query)
            .await
            .into_iter()
            .filter(|endpoint| {
                !endpoint.is_publisher
                    && endpoint.kind == TopicKind::Topic
                    && endpoint.name == topic
            })
            .collect()
    }

    /// How many publications `topic` has.
    pub async fn count_publishers(&self, topic: &str) -> usize {
        self.publishers_of(topic, GraphQuery::new().with_hidden())
            .await
            .len()
    }

    /// How many subscriptions `topic` has.
    pub async fn count_subscribers(&self, topic: &str) -> usize {
        self.subscribers_of(topic, GraphQuery::new().with_hidden())
            .await
            .len()
    }

    /// Every endpoint one node owns.
    pub async fn endpoints_of_node(&self, node: &str, query: GraphQuery) -> Vec<EndpointInfo> {
        self.endpoints(query)
            .await
            .into_iter()
            .filter(|endpoint| endpoint.node == node)
            .collect()
    }

    /// Wait until `node` appears on the graph.
    ///
    /// # Errors
    ///
    /// [`crate::Ros2Error::Timeout`].
    pub async fn wait_for_node(&self, node: &str, timeout: StdDuration) -> Ros2Result<()> {
        crate::pubsub::wait_for(
            &self.context,
            timeout,
            "waiting for a node to appear on the graph",
            || async {
                self.node_names(GraphQuery::new().with_hidden())
                    .await
                    .iter()
                    .any(|name| name == node)
            },
        )
        .await
    }

    /// Wait until `topic` has at least `count` publications.
    ///
    /// # Errors
    ///
    /// [`crate::Ros2Error::Timeout`].
    pub async fn wait_for_publishers(
        &self,
        topic: &str,
        count: usize,
        timeout: StdDuration,
    ) -> Ros2Result<()> {
        crate::pubsub::wait_for(
            &self.context,
            timeout,
            "waiting for a publisher to appear on the graph",
            || async { self.count_publishers(topic).await >= count },
        )
        .await
    }

    /// Stop the background task and delete the subscription.
    ///
    /// # Errors
    ///
    /// [`crate::Ros2Error::Rtps`] or [`crate::Ros2Error::Cdr`].
    pub async fn shutdown(&self) -> Ros2Result<()> {
        if let Some(task) = self.task.lock().await.take() {
            task.abort();
        }
        self.subscription.destroy().await?;
        Ok(())
    }

    /// Names and types for whichever endpoint kinds `keep` accepts.
    async fn names_and_types<F>(&self, query: GraphQuery, keep: F) -> BTreeMap<String, Vec<String>>
    where
        F: Fn(TopicKind) -> bool,
    {
        let mut found: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for endpoint in self.endpoints(query).await {
            if !keep(endpoint.kind) {
                continue;
            }
            let type_name = strip_service_suffix(&endpoint.type_name);
            found.entry(endpoint.name).or_default().insert(type_name);
        }
        found
            .into_iter()
            .map(|(name, types)| (name, types.into_iter().collect()))
            .collect()
    }

    /// Endpoint GID → owning node, from every graph sample.
    async fn owner_index(&self, query: GraphQuery) -> BTreeMap<Guid, String> {
        let mut owners = BTreeMap::new();
        for info in self.remote.lock().await.values() {
            index_participant(info, &mut owners);
        }
        if query.include_local
            && let Some(announcer) = self.context.announcer()
        {
            index_participant(&announcer.snapshot().await, &mut owners);
        }
        owners
    }

    /// Fold every incoming `ros_discovery_info` sample into the map.
    async fn pump(&self) {
        loop {
            let (payload, info) = self.subscription.recv().await;
            if !info.is_alive {
                // A disposal: the participant is going. Its entry goes with
                // it, so the graph does not keep a departed node.
                self.remote
                    .lock()
                    .await
                    .remove(&info.publisher.participant_guid());
                continue;
            }
            match ParticipantEntitiesInfo::decode(&payload, self.compat) {
                Ok(sample) => {
                    let key = sample
                        .participant()
                        .unwrap_or_else(|| info.publisher.participant_guid());
                    self.remote.lock().await.insert(key, sample);
                }
                Err(error) => tracing::debug!(
                    %error,
                    publisher = %info.publisher,
                    "dropping a ros_discovery_info sample that would not decode"
                ),
            }
        }
    }
}

/// Add one participant's node→endpoint mapping to `owners`.
///
/// Keyed by [`Guid`] rather than by [`Gid`]: a GID's *width* is the
/// distribution's business, and a Humble peer announcing twenty-four-octet
/// GIDs must still be joinable against an endpoint this participant knows
/// as a sixteen-octet GUID. Keying on the GID would make that lookup miss
/// every time, which is the exact bug the width switch exists to prevent.
fn index_participant(info: &ParticipantEntitiesInfo, owners: &mut BTreeMap<Guid, String>) {
    for node in &info.nodes {
        let name = node.fully_qualified();
        for gid in node.reader_gids.iter().chain(node.writer_gids.iter()) {
            if let Some(guid) = gid.guid() {
                owners.insert(guid, name.clone());
            }
        }
    }
}

/// Turn one endpoint's DDS names into ROS ones, or `None` when it is not a
/// ROS endpoint at all.
fn describe(
    guid: Guid,
    dds_topic: &str,
    dds_type: &str,
    is_publisher: bool,
    is_local: bool,
    compat: RosCompat,
    owners: &BTreeMap<Guid, String>,
) -> Option<EndpointInfo> {
    let (kind, name) = ros_topic_name(dds_topic)?;
    let gid = Gid::new(compat, guid);
    Some(EndpointInfo {
        node: owners.get(&guid).cloned().unwrap_or_default(),
        name,
        type_name: demangle_type_name(dds_type).unwrap_or_else(|| dds_type.to_owned()),
        gid,
        is_publisher,
        is_local,
        kind,
    })
}

/// The service type behind a request or response type.
///
/// `std_srvs/srv/Trigger_Request` is `std_srvs/srv/Trigger`; a plain message
/// type is returned unchanged.
fn strip_service_suffix(type_name: &str) -> String {
    for suffix in ["_Request", "_Response"] {
        if let Some(stripped) = type_name.strip_suffix(suffix) {
            return stripped.to_owned();
        }
    }
    type_name.to_owned()
}

/// The action type behind a `send_goal` request type.
///
/// `example_interfaces/action/Fibonacci_SendGoal_Request` is
/// `example_interfaces/action/Fibonacci`.
fn action_type_of(type_name: &str) -> Option<String> {
    type_name
        .strip_suffix("_SendGoal_Request")
        .or_else(|| type_name.strip_suffix("_SendGoal_Response"))
        .map(str::to_owned)
}

/// Read the `enclave=…;` a ROS 2 participant puts in `PID_USER_DATA`.
fn parse_enclave(user_data: &[u8]) -> Option<String> {
    let text = core::str::from_utf8(user_data).ok()?;
    let rest = text.strip_prefix("enclave=")?;
    let enclave = rest.strip_suffix(';').unwrap_or(rest);
    Some(enclave.to_owned())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::graph::wire::NodeEntitiesInfo;
    use astrs_rtps::structure::{EntityId, EntityKind, GuidPrefix, VendorId};

    fn guid(seed: u8, key: u32) -> Guid {
        Guid::new(
            GuidPrefix::vendor_scoped(VendorId::ASTRS, [seed; 10]),
            EntityId::user_defined(key, EntityKind::USER_WRITER_NO_KEY),
        )
    }

    #[test]
    fn a_query_defaults_to_local_and_visible() {
        let query = GraphQuery::new();
        assert!(query.include_local);
        assert!(!query.include_hidden);
        assert!(query.accepts("/chatter"));
        assert!(!query.accepts("/_ros2cli_1/get_parameters"));
        assert!(query.with_hidden().accepts("/_ros2cli_1/get_parameters"));
        assert!(!query.remote_only().include_local);
        assert!(!GraphQuery::default().include_hidden);
    }

    #[test]
    fn describing_an_endpoint_demangles_both_names() {
        let owners = BTreeMap::new();
        let info = describe(
            guid(1, 1),
            "rt/chatter",
            "std_msgs::msg::dds_::String_",
            true,
            false,
            RosCompat::Jazzy,
            &owners,
        )
        .expect("a ROS endpoint");
        assert_eq!(info.name, "/chatter");
        assert_eq!(info.type_name, "std_msgs/msg/String");
        assert_eq!(info.kind, TopicKind::Topic);
        assert!(info.is_publisher);
        assert!(!info.is_local);
        assert!(info.is_unclaimed());
    }

    #[test]
    fn a_non_ros_endpoint_is_not_described_at_all() {
        let owners = BTreeMap::new();
        assert!(
            describe(
                guid(1, 1),
                "DCPSParticipant",
                "ParticipantBuiltinTopicData",
                true,
                false,
                RosCompat::Jazzy,
                &owners,
            )
            .is_none(),
            "a builtin DDS topic is not a ROS topic"
        );
    }

    #[test]
    fn an_owned_endpoint_names_its_node() {
        let mut node = NodeEntitiesInfo::new("/robot", "talker");
        node.add_writer(Gid::new(RosCompat::Jazzy, guid(1, 1)));
        let mut info = ParticipantEntitiesInfo::for_participant(RosCompat::Jazzy, guid(1, 0));
        info.upsert(node);

        let mut owners = BTreeMap::new();
        index_participant(&info, &mut owners);

        let described = describe(
            guid(1, 1),
            "rt/chatter",
            "std_msgs::msg::dds_::String_",
            true,
            false,
            RosCompat::Jazzy,
            &owners,
        )
        .expect("a ROS endpoint");
        assert_eq!(described.node, "/robot/talker");
        assert!(!described.is_unclaimed());
    }

    #[test]
    fn a_service_type_loses_its_half_suffix() {
        assert_eq!(
            strip_service_suffix("std_srvs/srv/Trigger_Request"),
            "std_srvs/srv/Trigger"
        );
        assert_eq!(
            strip_service_suffix("std_srvs/srv/Trigger_Response"),
            "std_srvs/srv/Trigger"
        );
        assert_eq!(
            strip_service_suffix("std_msgs/msg/String"),
            "std_msgs/msg/String"
        );
    }

    #[test]
    fn an_action_type_is_derived_from_its_send_goal_service() {
        assert_eq!(
            action_type_of("example_interfaces/action/Fibonacci_SendGoal_Request").as_deref(),
            Some("example_interfaces/action/Fibonacci")
        );
        assert_eq!(
            action_type_of("example_interfaces/action/Fibonacci_SendGoal_Response").as_deref(),
            Some("example_interfaces/action/Fibonacci")
        );
        assert_eq!(action_type_of("std_msgs/msg/String"), None);
    }

    #[test]
    fn the_enclave_is_read_out_of_the_user_data() {
        assert_eq!(parse_enclave(b"enclave=/;").as_deref(), Some("/"));
        assert_eq!(
            parse_enclave(b"enclave=/robot/arm;").as_deref(),
            Some("/robot/arm")
        );
        assert_eq!(
            parse_enclave(b"enclave=/no/semicolon").as_deref(),
            Some("/no/semicolon"),
            "a peer that omits the terminator is still readable"
        );
        assert_eq!(parse_enclave(b"something else"), None);
        assert_eq!(parse_enclave(&[0xff, 0xfe]), None);
    }

    #[test]
    fn a_node_info_joins_and_hides_the_way_ros_does() {
        let node = NodeInfo {
            name: "talker".to_owned(),
            namespace: "/robot".to_owned(),
            participant: guid(1, 0),
            is_local: false,
            enclave: Some("/".to_owned()),
        };
        assert_eq!(node.fully_qualified(), "/robot/talker");
        assert!(!node.is_hidden());

        let hidden = NodeInfo {
            name: "_ros2cli_31337".to_owned(),
            namespace: "/".to_owned(),
            ..node
        };
        assert!(hidden.is_hidden());
    }

    #[test]
    fn node_infos_sort_by_namespace_then_name() {
        let mut nodes = [
            NodeInfo {
                name: "z".to_owned(),
                namespace: "/a".to_owned(),
                participant: guid(1, 0),
                is_local: false,
                enclave: None,
            },
            NodeInfo {
                name: "a".to_owned(),
                namespace: "/a".to_owned(),
                participant: guid(1, 0),
                is_local: false,
                enclave: None,
            },
        ];
        nodes.sort();
        assert_eq!(nodes[0].name, "a");
    }
}
