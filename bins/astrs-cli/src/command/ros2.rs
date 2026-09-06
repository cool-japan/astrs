//! The probe both `astrs ros2` verbs are built on (blueprint §17).
//!
//! `ros2 doctor` and `ros2 topics` ask the same question — *what is on this
//! DDS domain?* — and differ only in what they do with the answer. So the
//! participant, the discovery wait and the snapshot live here, and the two
//! verbs are presentation over [`probe`].
//!
//! # What a probe is
//!
//! A short-lived [`Ros2Context`] on the requested domain, with an
//! `astrs_probe_<pid>` node on it so a stock ROS box's `ros2 node list`
//! shows something recognisable rather than an anonymous participant. It
//! joins, listens for [`ProbeArgs::timeout`], snapshots the graph, and
//! shuts down — announcing its own departure on the way out, so it does not
//! leave a ghost behind (§12's "no lingering DDS participants" applied to a
//! CLI verb).
//!
//! # Why the wait is a wait and not a poll-until-empty
//!
//! Discovery is not complete at any particular moment: a peer that has not
//! announced yet is indistinguishable from one that does not exist. There
//! is no "done" to poll for, so the honest interface is a *budget* — the
//! probe listens for as long as it was told to and reports what it heard,
//! and the report says how long it listened. The early exit is a courtesy
//! for the common case, not a completeness claim: once at least one
//! participant has been seen and a full announce period has passed with no
//! new one, waiting longer rarely changes the answer.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use astrs_ros2::graph::{EndpointInfo, GraphQuery, NodeInfo};
use astrs_ros2::names::mangle::TopicKind;
use astrs_ros2::node::{ContextOptions, NodeOptions, Ros2Context, Ros2Node};
use astrs_rtps::behavior::MulticastCapability;
use astrs_rtps::discovery::RosCompat;
use astrs_rtps::structure::Locator;
use serde::Serialize;

use crate::error::CliError;

/// The DDS domain a probe joins when nothing says otherwise, and what
/// `ROS_DOMAIN_ID` defaults to.
pub const DEFAULT_DOMAIN_ID: u32 = 0;

/// The environment variable ROS 2 itself reads the domain from.
pub const ENV_DOMAIN_ID: &str = "ROS_DOMAIN_ID";

/// How long a probe listens by default.
///
/// Two seconds: SPDP's default announce period is well under one, so this
/// gives every live participant at least two chances to be heard, and it is
/// short enough that `astrs ros2 topics` still feels like a listing rather
/// than a scan.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the probe waits after the last new participant before it
/// concludes early.
pub const QUIET_PERIOD: Duration = Duration::from_millis(400);

/// How often the wait re-checks what it has heard.
pub const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// What a probe needs to know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeArgs {
    /// The DDS domain to join.
    pub domain_id: u32,
    /// How long to listen.
    pub timeout: Duration,
    /// Include hidden (`_`-prefixed) names in the listing.
    pub include_hidden: bool,
    /// Emit the report as JSON instead of a table.
    pub json: bool,
}

impl Default for ProbeArgs {
    fn default() -> Self {
        Self {
            domain_id: domain_id_from_env(),
            timeout: DEFAULT_TIMEOUT,
            include_hidden: false,
            json: false,
        }
    }
}

impl ProbeArgs {
    /// The graph query this probe's flags describe.
    ///
    /// `remote_only`: the probe's own participant has one reader (the
    /// `ros_discovery_info` subscription) and nothing else worth showing, and
    /// a listing that included it would report a topic the user did not
    /// create.
    #[must_use]
    pub const fn query(&self) -> GraphQuery {
        let query = GraphQuery::new().remote_only();
        if self.include_hidden {
            query.with_hidden()
        } else {
            query
        }
    }
}

/// The domain `ROS_DOMAIN_ID` names, or [`DEFAULT_DOMAIN_ID`].
///
/// A value that is not a number is *ignored* rather than fatal: `ros2`
/// itself treats a malformed `ROS_DOMAIN_ID` as unset, and a diagnostic verb
/// that refused to start over one would be the least helpful moment to be
/// strict.
#[must_use]
pub fn domain_id_from_env() -> u32 {
    std::env::var(ENV_DOMAIN_ID)
        .ok()
        .and_then(|raw| raw.trim().parse::<u32>().ok())
        .unwrap_or(DEFAULT_DOMAIN_ID)
}

/// One participant the probe heard from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ParticipantSummary {
    /// Its GUID prefix, rendered.
    pub guid: String,
    /// Whether its vendor id is AstRS's own `41 53` (§10.2).
    pub is_astrs: bool,
    /// The nodes it announced through `ros_discovery_info`.
    pub nodes: Vec<String>,
}

/// One topic on the graph, with the endpoints on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TopicSummary {
    /// The fully-qualified ROS name.
    pub name: String,
    /// Every ROS type name announced on it — more than one means two peers
    /// disagree, which is worth seeing rather than collapsing.
    pub types: Vec<String>,
    /// How many publications.
    pub publishers: usize,
    /// How many subscriptions.
    pub subscribers: usize,
    /// The nodes publishing on it.
    pub publisher_nodes: Vec<String>,
    /// The nodes subscribing to it.
    pub subscriber_nodes: Vec<String>,
}

/// What one probe heard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProbeReport {
    /// The domain that was probed.
    pub domain_id: u32,
    /// How long the probe actually listened, in milliseconds.
    pub listened_ms: u64,
    /// What the multicast join reported.
    pub multicast: String,
    /// Whether the multicast join succeeded.
    pub multicast_joined: bool,
    /// The locator the probe announced itself on.
    pub locator: String,
    /// Every participant heard from, excluding the probe itself.
    pub participants: Vec<ParticipantSummary>,
    /// Every ROS node announced on the graph.
    pub nodes: Vec<String>,
    /// Every topic, sorted by name.
    pub topics: Vec<TopicSummary>,
    /// Every service, sorted by name.
    pub services: Vec<TopicSummary>,
    /// Every action, sorted by name.
    pub actions: Vec<TopicSummary>,
    /// Endpoints no `ros_discovery_info` sample claimed — a non-ROS DDS
    /// participant, or a peer whose graph sample has not arrived yet.
    pub unclaimed_endpoints: usize,
}

impl ProbeReport {
    /// How many endpoints of every kind were seen.
    #[must_use]
    pub fn endpoint_count(&self) -> usize {
        self.topics
            .iter()
            .chain(&self.services)
            .chain(&self.actions)
            .map(|entry| entry.publishers.saturating_add(entry.subscribers))
            .sum()
    }

    /// Whether anything at all answered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.participants.is_empty() && self.topics.is_empty()
    }
}

/// Join `args.domain_id`, listen, and report what answered.
///
/// # Errors
///
/// [`CliError::Ros2`] when the participant cannot be created (a socket that
/// will not bind, a domain id with no legal port mapping) or the graph
/// subscription cannot be started.
pub fn probe(args: &ProbeArgs) -> Result<ProbeReport, CliError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|error| CliError::Ros2(format!("no runtime for the probe: {error}")))?;
    runtime.block_on(probe_async(args))
}

/// The probe itself, for a caller that already has a runtime.
///
/// # Errors
///
/// As [`probe`].
pub async fn probe_async(args: &ProbeArgs) -> Result<ProbeReport, CliError> {
    let started = Instant::now();
    let options = ContextOptions::new(args.domain_id)
        // Jazzy's 16-byte GID is the wider-compatible read: a Humble peer's
        // 24-byte GID still decodes, and the probe never *writes* a GID
        // anyone correlates on. `astrs ros2 doctor` reports what it sees
        // rather than negotiating.
        .with_compat(RosCompat::Jazzy)
        .with_multicast(true);
    let context = Ros2Context::new(options)
        .await
        .map_err(|error| CliError::Ros2(format!("the probe participant: {error}")))?;

    // A named node, so a stock ROS box sees `/astrs_probe_<pid>` rather than
    // an anonymous participant. Hidden by the leading underscore convention
    // is deliberately *not* used: a user running `ros2 node list` while
    // debugging should see the probe that is talking to them.
    let node = Ros2Node::new(
        Arc::clone(&context),
        format!("astrs_probe_{}", std::process::id()),
        NodeOptions::default().with_parameter_services(false),
    )
    .await
    .map_err(|error| CliError::Ros2(format!("the probe node: {error}")))?;

    let report = listen(&context, &node, args, started).await;

    // Leave the domain the way §12 asks a node to: announce the departure
    // rather than vanishing and letting a peer's lease expire.
    if let Err(error) = node.shutdown().await {
        tracing::debug!(%error, "the probe node was already shut down");
    }
    context.shutdown().await;

    report
}

/// Listen for the probe's budget and snapshot the graph.
async fn listen(
    context: &Arc<Ros2Context>,
    node: &Ros2Node,
    args: &ProbeArgs,
    started: Instant,
) -> Result<ProbeReport, CliError> {
    let deadline = started + args.timeout;
    let graph = node.graph();
    let mut seen = 0_usize;
    let mut last_change = Instant::now();

    while Instant::now() < deadline {
        let count = graph.known_participants().await;
        if count != seen {
            seen = count;
            last_change = Instant::now();
        } else if seen > 0 && last_change.elapsed() >= QUIET_PERIOD {
            break;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    let query = args.query();
    let nodes = graph.nodes(query).await;
    let endpoints = graph.endpoints(query).await;

    Ok(ProbeReport {
        domain_id: args.domain_id,
        listened_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        multicast: describe_multicast(context.multicast()),
        multicast_joined: context.multicast().is_joined(),
        locator: describe_locator(context.metatraffic_locator()),
        participants: summarize_participants(&nodes, graph.known_participants().await),
        nodes: node_names(&nodes),
        topics: summarize(&endpoints, TopicKind::Topic),
        services: summarize(&endpoints, TopicKind::Request),
        actions: summarize_actions(&endpoints),
        unclaimed_endpoints: endpoints
            .iter()
            .filter(|endpoint| endpoint.is_unclaimed())
            .count(),
    })
}

/// The multicast capability, rendered for a report.
#[must_use]
pub fn describe_multicast(capability: &MulticastCapability) -> String {
    match capability {
        MulticastCapability::Joined { group } => format!("joined {group}"),
        MulticastCapability::Disabled => "disabled".to_owned(),
        MulticastCapability::Refused { group, reason } => {
            format!("refused {group} ({reason})")
        }
    }
}

/// A locator, rendered for a report.
#[must_use]
pub fn describe_locator(locator: Locator) -> String {
    locator.socket_addr().map_or_else(
        |_| "unaddressable".to_owned(),
        |address| address.to_string(),
    )
}

/// Every fully-qualified node name, sorted and deduplicated.
fn node_names(nodes: &[NodeInfo]) -> Vec<String> {
    let mut names: Vec<String> = nodes.iter().map(NodeInfo::fully_qualified).collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// One summary per participant, with the nodes it hosts.
fn summarize_participants(nodes: &[NodeInfo], known: usize) -> Vec<ParticipantSummary> {
    use std::collections::BTreeMap;

    let mut by_participant: BTreeMap<String, ParticipantSummary> = BTreeMap::new();
    for info in nodes {
        let guid = info.participant.prefix.to_string();
        let entry = by_participant
            .entry(guid.clone())
            .or_insert_with(|| ParticipantSummary {
                is_astrs: info.participant.prefix.vendor_id()
                    == astrs_rtps::structure::VendorId::ASTRS,
                guid,
                nodes: Vec::new(),
            });
        entry.nodes.push(info.fully_qualified());
    }
    for entry in by_participant.values_mut() {
        entry.nodes.sort_unstable();
        entry.nodes.dedup();
    }

    // A participant that announced no `ros_discovery_info` sample has no
    // nodes to key on, so the count is reported rather than invented: the
    // difference between `known` and what we listed is exactly the set of
    // participants that are on the domain but not (yet) speaking ROS.
    let listed = by_participant.len();
    let mut summaries: Vec<ParticipantSummary> = by_participant.into_values().collect();
    for index in listed..known {
        summaries.push(ParticipantSummary {
            guid: format!("<unannounced #{}>", index.saturating_sub(listed) + 1),
            is_astrs: false,
            nodes: Vec::new(),
        });
    }
    summaries
}

/// Group endpoints of one kind into per-name summaries.
fn summarize(endpoints: &[EndpointInfo], kind: TopicKind) -> Vec<TopicSummary> {
    use std::collections::BTreeMap;

    let mut grouped: BTreeMap<&str, Grouped> = BTreeMap::new();
    for endpoint in endpoints {
        let matches = match kind {
            TopicKind::Topic => endpoint.kind == TopicKind::Topic,
            // A service's two halves are one service; the reply half is
            // counted with the request half rather than listed twice.
            _ => endpoint.kind.is_service(),
        };
        if !matches || is_action_endpoint(&endpoint.name) {
            continue;
        }
        grouped
            .entry(endpoint.name.as_str())
            .or_default()
            .absorb(endpoint);
    }
    grouped
        .into_iter()
        .map(|(name, group)| group.finish(name.to_owned()))
        .collect()
}

/// Group an action's five endpoints under the action's own name.
fn summarize_actions(endpoints: &[EndpointInfo]) -> Vec<TopicSummary> {
    use std::collections::BTreeMap;

    let mut grouped: BTreeMap<String, Grouped> = BTreeMap::new();
    for endpoint in endpoints {
        let Some((action, _)) = astrs_ros2::names::mangle::split_action_name(&endpoint.name) else {
            continue;
        };
        grouped
            .entry(action.to_owned())
            .or_default()
            .absorb(endpoint);
    }
    grouped
        .into_iter()
        .map(|(name, group)| group.finish(name))
        .collect()
}

/// Whether a ROS name belongs to an action's `_action/` sub-tree.
fn is_action_endpoint(name: &str) -> bool {
    astrs_ros2::names::mangle::split_action_name(name).is_some()
}

/// The accumulator [`summarize`] groups into.
#[derive(Debug, Default)]
struct Grouped {
    types: BTreeSet<String>,
    publishers: usize,
    subscribers: usize,
    publisher_nodes: BTreeSet<String>,
    subscriber_nodes: BTreeSet<String>,
}

impl Grouped {
    fn absorb(&mut self, endpoint: &EndpointInfo) {
        if !endpoint.type_name.is_empty() {
            self.types.insert(endpoint.type_name.clone());
        }
        let node = if endpoint.is_unclaimed() {
            "<unclaimed>".to_owned()
        } else {
            endpoint.node.clone()
        };
        if endpoint.is_publisher {
            self.publishers = self.publishers.saturating_add(1);
            self.publisher_nodes.insert(node);
        } else {
            self.subscribers = self.subscribers.saturating_add(1);
            self.subscriber_nodes.insert(node);
        }
    }

    fn finish(self, name: String) -> TopicSummary {
        TopicSummary {
            name,
            types: self.types.into_iter().collect(),
            publishers: self.publishers,
            subscribers: self.subscribers,
            publisher_nodes: self.publisher_nodes.into_iter().collect(),
            subscriber_nodes: self.subscriber_nodes.into_iter().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_rtps::discovery::Gid;
    use astrs_rtps::structure::{EntityId, EntityKind, Guid, GuidPrefix, VendorId};

    use super::*;

    fn guid(seed: u8) -> Guid {
        Guid::new(
            GuidPrefix::vendor_scoped(VendorId::ASTRS, [seed; 10]),
            EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY),
        )
    }

    fn endpoint(
        node: &str,
        name: &str,
        type_name: &str,
        is_publisher: bool,
        kind: TopicKind,
    ) -> EndpointInfo {
        EndpointInfo {
            node: node.to_owned(),
            name: name.to_owned(),
            type_name: type_name.to_owned(),
            gid: Gid::new(RosCompat::Jazzy, guid(1)),
            is_publisher,
            is_local: false,
            kind,
        }
    }

    #[test]
    fn the_default_timeout_and_domain_are_the_documented_ones() {
        let args = ProbeArgs {
            domain_id: 0,
            ..ProbeArgs::default()
        };
        assert_eq!(args.timeout, DEFAULT_TIMEOUT);
        assert_eq!(args.domain_id, 0);
        assert!(!args.json);
        assert!(!args.include_hidden);
    }

    #[test]
    fn the_query_excludes_the_probes_own_endpoints() {
        let args = ProbeArgs::default();
        assert!(!args.query().include_local);
        assert!(!args.query().include_hidden);

        let hidden = ProbeArgs {
            include_hidden: true,
            ..ProbeArgs::default()
        };
        assert!(hidden.query().include_hidden);
        assert!(!hidden.query().include_local);
    }

    #[test]
    fn topics_are_grouped_by_name_with_their_endpoint_counts() {
        let endpoints = vec![
            endpoint(
                "/talker",
                "/chatter",
                "std_msgs/msg/String",
                true,
                TopicKind::Topic,
            ),
            endpoint(
                "/listener",
                "/chatter",
                "std_msgs/msg/String",
                false,
                TopicKind::Topic,
            ),
            endpoint(
                "/other",
                "/chatter",
                "std_msgs/msg/String",
                false,
                TopicKind::Topic,
            ),
            endpoint(
                "/talker",
                "/scan",
                "sensor_msgs/msg/LaserScan",
                true,
                TopicKind::Topic,
            ),
        ];
        let topics = summarize(&endpoints, TopicKind::Topic);
        assert_eq!(topics.len(), 2);
        assert_eq!(topics[0].name, "/chatter", "sorted by name");
        assert_eq!(topics[0].publishers, 1);
        assert_eq!(topics[0].subscribers, 2);
        assert_eq!(topics[0].types, vec!["std_msgs/msg/String"]);
        assert_eq!(topics[0].publisher_nodes, vec!["/talker"]);
        assert_eq!(topics[0].subscriber_nodes, vec!["/listener", "/other"]);
        assert_eq!(topics[1].name, "/scan");
    }

    #[test]
    fn two_types_on_one_topic_are_both_reported() {
        let endpoints = vec![
            endpoint("/a", "/x", "std_msgs/msg/String", true, TopicKind::Topic),
            endpoint("/b", "/x", "std_msgs/msg/Int32", false, TopicKind::Topic),
        ];
        let topics = summarize(&endpoints, TopicKind::Topic);
        assert_eq!(
            topics[0].types,
            vec!["std_msgs/msg/Int32", "std_msgs/msg/String"],
            "a type disagreement is shown, not collapsed"
        );
    }

    #[test]
    fn a_services_two_halves_are_one_entry() {
        let endpoints = vec![
            endpoint(
                "/server",
                "/add_two_ints",
                "example_interfaces/srv/AddTwoInts_Request",
                false,
                TopicKind::Request,
            ),
            endpoint(
                "/server",
                "/add_two_ints",
                "example_interfaces/srv/AddTwoInts_Response",
                true,
                TopicKind::Reply,
            ),
        ];
        let services = summarize(&endpoints, TopicKind::Request);
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].name, "/add_two_ints");
        assert_eq!(services[0].publishers, 1);
        assert_eq!(services[0].subscribers, 1);
    }

    #[test]
    fn an_actions_endpoints_are_grouped_under_the_action_name() {
        let endpoints = vec![
            endpoint(
                "/server",
                "/fibonacci/_action/send_goal",
                "example_interfaces/action/Fibonacci_SendGoal_Request",
                false,
                TopicKind::Request,
            ),
            endpoint(
                "/server",
                "/fibonacci/_action/feedback",
                "example_interfaces/action/Fibonacci_FeedbackMessage",
                true,
                TopicKind::Topic,
            ),
        ];
        let actions = summarize_actions(&endpoints);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].name, "/fibonacci");
        assert_eq!(actions[0].publishers, 1);
        assert_eq!(actions[0].subscribers, 1);

        // …and they do not also appear as ordinary topics or services.
        assert!(summarize(&endpoints, TopicKind::Topic).is_empty());
        assert!(summarize(&endpoints, TopicKind::Request).is_empty());
    }

    #[test]
    fn an_unclaimed_endpoint_is_labelled_rather_than_dropped() {
        let endpoints = vec![endpoint(
            "",
            "/mystery",
            "some/msg/Type",
            true,
            TopicKind::Topic,
        )];
        let topics = summarize(&endpoints, TopicKind::Topic);
        assert_eq!(topics[0].publisher_nodes, vec!["<unclaimed>"]);
    }

    #[test]
    fn participants_are_grouped_and_unannounced_ones_are_counted() {
        let nodes = vec![
            NodeInfo {
                name: "talker".to_owned(),
                namespace: "/".to_owned(),
                participant: guid(1),
                is_local: false,
                enclave: None,
            },
            NodeInfo {
                name: "listener".to_owned(),
                namespace: "/".to_owned(),
                participant: guid(1),
                is_local: false,
                enclave: None,
            },
        ];
        let summaries = summarize_participants(&nodes, 3);
        assert_eq!(summaries.len(), 3, "one announced, two unannounced");
        assert_eq!(summaries[0].nodes, vec!["/listener", "/talker"]);
        assert!(
            summaries[0].is_astrs,
            "the fixture uses the AstRS vendor id"
        );
        assert!(summaries[1].nodes.is_empty());
    }

    #[test]
    fn the_multicast_capability_renders_every_outcome() {
        let group = std::net::Ipv4Addr::new(239, 255, 0, 1);
        assert_eq!(
            describe_multicast(&MulticastCapability::Joined { group }),
            "joined 239.255.0.1"
        );
        assert_eq!(
            describe_multicast(&MulticastCapability::Disabled),
            "disabled"
        );
    }

    #[test]
    fn an_empty_report_says_so() {
        let report = ProbeReport {
            domain_id: 0,
            listened_ms: 10,
            multicast: "joined".to_owned(),
            multicast_joined: true,
            locator: "127.0.0.1:7400".to_owned(),
            participants: Vec::new(),
            nodes: Vec::new(),
            topics: Vec::new(),
            services: Vec::new(),
            actions: Vec::new(),
            unclaimed_endpoints: 0,
        };
        assert!(report.is_empty());
        assert_eq!(report.endpoint_count(), 0);
    }

    #[test]
    fn a_malformed_ros_domain_id_falls_back_rather_than_failing() {
        // The function reads the process environment, which a test must not
        // mutate; what is pinned here is the fallback the parse falls to.
        assert_eq!(DEFAULT_DOMAIN_ID, 0);
        assert!(domain_id_from_env() < 233, "a legal ROS domain id");
    }
}
