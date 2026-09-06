//! The bridge against a second, independent RTPS participant, over real
//! UDP sockets — §10.2's in-repo interoperability shape, applied to §10.5.
//!
//! Nothing here is mocked on the ROS 2 side: the peer is a real
//! [`Ros2Context`] with its own participant, its own sockets and its own
//! SPDP/SEDP exchange, and the bridge finds it exactly the way a deployment
//! does. What *is* mocked is the daemon — [`MockDaemon`] hands the bridge a
//! `(Node, EventStream)` pair identical to the one the real handshake
//! produces — so the whole of §10.5's declarative path runs, from the
//! `ros2:` YAML through the plan, the type resolution, the codec, the
//! endpoints and the event loop.
//!
//! # Discovery, deterministically
//!
//! Every participant here binds port 0 on loopback and finds its peer
//! through an explicit initial peer, following
//! `crates/astrs-rtps/tests/harness/mod.rs`'s three rules: ephemeral ports
//! so parallel tests cannot collide, unicast so no kernel multicast
//! permission is needed (a sandboxed macOS host refuses the join), and no
//! fixed sleeps — every wait polls a condition under a generous bound.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use astrs_cdr::Encoding;
use astrs_data::AstrsMessage;
use astrs_manifest::Ros2Config;
use astrs_node_api::Payload;
use astrs_node_api::testing::{MockDaemon, RecordedSend};
use astrs_ros2::msg::builtin_interfaces::Time;
use astrs_ros2::msg::geometry_msgs::{Twist, Vector3};
use astrs_ros2::msg::sensor_msgs::LaserScan;
use astrs_ros2::msg::std_msgs::Header;
use astrs_ros2::names::FullName;
use astrs_ros2::node::{ContextOptions, NodeOptions, Ros2Context, Ros2Node};
use astrs_ros2::pubsub::{RawPublisher, RawSubscription};
use astrs_ros2::qos::{QosProfile, Reliability};
use astrs_ros2_bridge_node::config::BridgeSettings;
use astrs_ros2_bridge_node::run::run_with;
use astrs_time::HlcTimestamp;
use astrs_wire::{
    DataId, InputSpec, Metadata, NodeId, NodeSource, NodeSpawnSpec, OutputSpec, PortRef, StopCause,
};

/// How long any wait may take before the test fails.
///
/// Generous on purpose: this bounds a *failure*. A working exchange over
/// loopback completes in milliseconds.
const PATIENCE: Duration = Duration::from_secs(10);

/// How often a poll re-checks its condition.
const POLL: Duration = Duration::from_millis(5);

/// The `ros2:` block both directions are configured from.
const BRIDGE_YAML: &str = "\
compat: humble
topics:
  - topic: /scan
    message_type: sensor_msgs/msg/LaserScan
    direction: to_astrs
  - topic: /cmd_vel
    message_type: geometry_msgs/msg/Twist
    direction: from_astrs
qos: { reliable: true, keep_last: 10 }
";

/// The bridge node's name in every test here.
fn bridge_id() -> NodeId {
    NodeId::new("ros2-bridge").unwrap()
}

/// The spawn specification the daemon would hand this bridge.
fn bridge_spec(daemon: &MockDaemon, yaml: &str) -> (NodeSpawnSpec, Ros2Config) {
    let config: Ros2Config = astrs_yaml::from_str(yaml).expect("the fixture parses");
    let mut spec = NodeSpawnSpec::new(
        daemon.dataflow(),
        bridge_id(),
        0,
        NodeSource::Ros2Bridge {
            config: serde_json::to_string(&config).expect("the block serializes"),
        },
    );
    spec = spec.with_output(OutputSpec::new(DataId::new("scan").unwrap()));
    spec = spec.with_input(InputSpec::new(
        DataId::new("cmd_vel").unwrap(),
        PortRef::from_parts("planner", "cmd").unwrap(),
    ));
    (spec, config)
}

/// Settings that point the bridge at `peer` over unicast, with the
/// multicast join skipped.
///
/// Everything else is the shipped default — the `ros_discovery_info`
/// announcement included — so these tests exercise the configuration a
/// deployment actually gets rather than a trimmed-down one.
fn settings_for(peer: &Ros2Context) -> BridgeSettings {
    let address = peer
        .metatraffic_locator()
        .socket_addr()
        .expect("a bound participant has a dialable locator");
    BridgeSettings {
        multicast: false,
        announce_graph: true,
        initial_peers: vec![address.to_string()],
        ..BridgeSettings::default()
    }
}

/// A peer participant and a node on it, both on loopback with no multicast.
async fn peer_node(name: &str) -> (std::sync::Arc<Ros2Context>, Ros2Node) {
    let context = Ros2Context::new(ContextOptions::loopback())
        .await
        .expect("the peer participant binds");
    let node = Ros2Node::new(
        std::sync::Arc::clone(&context),
        name,
        NodeOptions::default().with_parameter_services(false),
    )
    .await
    .expect("the peer node starts");
    (context, node)
}

/// A `sensor_msgs/msg/LaserScan` with a recognisable stamp and ranges.
fn scan(sec: i32, nanosec: u32) -> LaserScan {
    LaserScan {
        header: Header {
            stamp: Time { sec, nanosec },
            frame_id: "laser".to_owned(),
        },
        angle_min: -1.5,
        angle_max: 1.5,
        angle_increment: 0.25,
        time_increment: 0.0,
        scan_time: 0.05,
        range_min: 0.1,
        range_max: 30.0,
        ranges: vec![1.5, 2.5, 3.5],
        intensities: vec![10.0, 20.0, 30.0],
    }
}

/// Poll until `node` has sent at least `count` messages on `output`.
async fn await_sends(daemon: &MockDaemon, output: &str, count: usize) -> Vec<RecordedSend> {
    let id = DataId::new(output).unwrap();
    let node = bridge_id();
    let deadline = Instant::now() + PATIENCE;
    loop {
        let sends = daemon.sends_on(&node, &id);
        if sends.len() >= count {
            return sends;
        }
        assert!(
            Instant::now() < deadline,
            "the bridge published {} of {count} messages on `{output}` within {PATIENCE:?}",
            sends.len()
        );
        tokio::time::sleep(POLL).await;
    }
}

/// Poll until `condition` holds.
async fn await_condition(label: &str, mut condition: impl AsyncFnMut() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if condition().await {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{label} did not happen within {PATIENCE:?}"
        );
        tokio::time::sleep(POLL).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_declared_bridge_round_trips_a_topic_in_each_direction() {
    let (peer_context, peer) = peer_node("peer").await;

    let daemon = MockDaemon::start().expect("the mock daemon starts");
    let (spec, config) = bridge_spec(&daemon, BRIDGE_YAML);
    let (node, events) = daemon.connect_node(spec).expect("the bridge registers");
    let settings = settings_for(&peer_context);

    let bridge = tokio::spawn(async move { run_with(node, events, &config, &settings).await });

    // ---- direction: to_astrs -------------------------------------------
    let publisher: RawPublisher = peer
        .create_raw_publisher(
            "/scan",
            "sensor_msgs::msg::dds_::LaserScan_",
            QosProfile::default(),
        )
        .await
        .expect("the peer publisher is created");
    publisher
        .wait_for_subscriptions(1, PATIENCE)
        .await
        .expect("the bridge's subscription must match the peer's publisher");

    let sent = scan(1_700_000_000, 250_000_000);
    publisher
        .publish_bytes(astrs_cdr::to_vec(&sent, Encoding::ROS2).unwrap())
        .await
        .expect("the peer publishes");

    let sends = await_sends(&daemon, "scan", 1).await;
    let recorded = &sends[0];
    let payload = Payload::inline(recorded.bytes().expect("inline payload").to_vec());
    let received =
        LaserScan::from_record_batch(payload.batch().expect("a columnar batch")).unwrap();
    assert_eq!(
        received, sent,
        "the CDR sample crossed as columns unchanged"
    );
    assert_eq!(
        recorded.metadata.timestamp,
        HlcTimestamp::new(1_700_000_000_250_000_000, 0),
        "§10.5: the ROS header stamp becomes the HLC metadata timestamp"
    );

    // ---- direction: from_astrs -----------------------------------------
    let subscription: RawSubscription = peer
        .create_raw_subscription(
            "/cmd_vel",
            "geometry_msgs::msg::dds_::Twist_",
            QosProfile::default(),
        )
        .await
        .expect("the peer subscription is created");
    await_condition(
        "the bridge's publisher matches the peer's subscription",
        async || subscription.publisher_count().await >= 1,
    )
    .await;

    let command = Twist {
        linear: Vector3 {
            x: 1.0,
            y: 2.0,
            z: 3.0,
        },
        angular: Vector3 {
            x: 0.25,
            y: 0.5,
            z: 0.75,
        },
    };
    let batch = command.to_record_batch().unwrap();
    let outbound = Payload::from_batch(batch).unwrap().to_vec();
    let stamp = HlcTimestamp::new(1_800_000_000_000_000_000, 0);
    daemon
        .send_input(
            &bridge_id(),
            &DataId::new("cmd_vel").unwrap(),
            Metadata::new(stamp),
            outbound,
        )
        .expect("the daemon delivers the input");

    let (octets, info) = subscription
        .recv_within(PATIENCE)
        .await
        .expect("the peer must receive the bridged command");
    let arrived: Twist = astrs_cdr::from_bytes_tolerant(&octets).unwrap();
    assert_eq!(
        arrived, command,
        "the columnar batch crossed as CDR unchanged"
    );
    assert_eq!(
        info.source_timestamp,
        Some(astrs_ros2::time::RosTime::new(1_800_000_000, 0)),
        "the graph's HLC becomes the DDS source timestamp"
    );

    // ---- clean shutdown -------------------------------------------------
    daemon
        .stop(&bridge_id(), StopCause::Requested)
        .expect("the daemon can stop the bridge");
    bridge
        .await
        .expect("the bridge task joins")
        .expect("the bridge exits cleanly on Stop");

    await_condition(
        "the bridge's endpoints leave the peer's graph",
        async || {
            publisher.subscription_count().await == 0 && subscription.publisher_count().await == 0
        },
    )
    .await;

    peer_context.shutdown().await;
    daemon.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_best_effort_peer_does_not_match_the_reliable_qos_the_manifest_asked_for() {
    let (peer_context, peer) = peer_node("strict_peer").await;

    let daemon = MockDaemon::start().expect("the mock daemon starts");
    let (spec, config) = bridge_spec(&daemon, BRIDGE_YAML);
    let (node, events) = daemon.connect_node(spec).expect("the bridge registers");
    let settings = settings_for(&peer_context);
    let bridge = tokio::spawn(async move { run_with(node, events, &config, &settings).await });

    // The manifest asked for `reliable: true`, so the bridge's `/scan`
    // subscription is RELIABLE. A BEST_EFFORT publisher cannot serve it —
    // that is the QoS request being *honoured*, not a discovery failure,
    // and a bridge that silently accepted the sample would be lying about
    // the delivery guarantee the graph was promised.
    let sensor = QosProfile::default().with_reliability(Reliability::BestEffort);
    assert_eq!(sensor.reliability, Reliability::BestEffort);
    let lax: RawPublisher = peer
        .create_raw_publisher("/scan", "sensor_msgs::msg::dds_::LaserScan_", sensor)
        .await
        .expect("the peer publisher is created");

    // A control publisher on the *same* topic with the profile the manifest
    // asked for proves the two participants really did discover each other,
    // so the negative result above is a QoS decision rather than silence.
    let strict: RawPublisher = peer
        .create_raw_publisher(
            "/scan",
            "sensor_msgs::msg::dds_::LaserScan_",
            QosProfile::default(),
        )
        .await
        .expect("the control publisher is created");
    strict
        .wait_for_subscriptions(1, PATIENCE)
        .await
        .expect("a reliable publisher must match the bridge's reliable subscription");

    assert_eq!(
        lax.subscription_count().await,
        0,
        "a best-effort publisher must not match a reliable subscription"
    );

    daemon
        .stop(&bridge_id(), StopCause::Requested)
        .expect("the daemon can stop the bridge");
    bridge
        .await
        .expect("the bridge task joins")
        .expect("the bridge exits cleanly");
    peer_context.shutdown().await;
    daemon.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unknown_message_type_fails_at_startup_by_name() {
    let daemon = MockDaemon::start().expect("the mock daemon starts");
    let yaml = "\
compat: humble
topic: /custom
message_type: my_msgs/msg/Custom
direction: to_astrs
";
    let config: Ros2Config = astrs_yaml::from_str(yaml).unwrap();
    let mut spec = NodeSpawnSpec::new(
        daemon.dataflow(),
        bridge_id(),
        0,
        NodeSource::Ros2Bridge {
            config: serde_json::to_string(&config).unwrap(),
        },
    );
    spec = spec.with_output(OutputSpec::new(DataId::new("custom").unwrap()));
    let (node, events) = daemon.connect_node(spec).expect("the bridge registers");

    let error = run_with(node, events, &config, &BridgeSettings::default())
        .await
        .expect_err("an unbridgeable type must fail at startup");

    let text = error.to_string();
    assert!(text.contains("my_msgs/msg/Custom"), "{text}");
    assert!(text.contains("AMENT_PREFIX_PATH"), "{text}");
    assert_eq!(
        error.exit_code(),
        78,
        "a configuration fault exits with EX_CONFIG"
    );
    assert!(error.is_startup());
    daemon.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_port_the_manifest_never_declared_fails_before_a_participant_is_created() {
    let daemon = MockDaemon::start().expect("the mock daemon starts");
    let config: Ros2Config = astrs_yaml::from_str(BRIDGE_YAML).unwrap();
    // The same block, but the node declares neither port.
    let spec = NodeSpawnSpec::new(
        daemon.dataflow(),
        bridge_id(),
        0,
        NodeSource::Ros2Bridge {
            config: serde_json::to_string(&config).unwrap(),
        },
    );
    let (node, events) = daemon.connect_node(spec).expect("the bridge registers");

    let error = run_with(node, events, &config, &BridgeSettings::default())
        .await
        .expect_err("a plan with no ports must fail");
    let text = error.to_string();
    assert!(text.contains("topic /scan"), "{text}");
    assert!(text.contains("no outputs at all"), "{text}");
    daemon.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_service_bridge_carries_a_request_and_its_response_by_request_id() {
    let (peer_context, peer) = peer_node("service_peer").await;

    let daemon = MockDaemon::start().expect("the mock daemon starts");
    let yaml = "\
compat: humble
service: /set_flag
message_type: std_srvs/srv/SetBool
role: server
";
    let config: Ros2Config = astrs_yaml::from_str(yaml).unwrap();
    let mut spec = NodeSpawnSpec::new(
        daemon.dataflow(),
        bridge_id(),
        0,
        NodeSource::Ros2Bridge {
            config: serde_json::to_string(&config).unwrap(),
        },
    );
    spec = spec.with_output(OutputSpec::new(DataId::new("request").unwrap()));
    spec = spec.with_input(InputSpec::new(
        DataId::new("response").unwrap(),
        PortRef::from_parts("handler", "out").unwrap(),
    ));
    let (node, events) = daemon.connect_node(spec).expect("the bridge registers");
    let settings = settings_for(&peer_context);
    let bridge = tokio::spawn(async move { run_with(node, events, &config, &settings).await });

    // A hand-rolled client: the request half is `rq/set_flagRequest`, which
    // is what `RawPublisher::for_kind` mangles a service name into.
    let name = FullName::service("/set_flag").unwrap();
    let request_writer = RawPublisher::for_kind(
        std::sync::Arc::clone(&peer_context),
        name.clone(),
        astrs_ros2::names::TopicKind::Request,
        "std_srvs::srv::dds_::SetBool_Request_",
        QosProfile::services_default(),
        Some(peer.fully_qualified_name()),
    )
    .await
    .unwrap();
    let reply_reader = RawSubscription::for_kind(
        std::sync::Arc::clone(&peer_context),
        name,
        astrs_ros2::names::TopicKind::Reply,
        "std_srvs::srv::dds_::SetBool_Response_",
        QosProfile::services_default(),
        Some(peer.fully_qualified_name()),
    )
    .await
    .unwrap();
    request_writer
        .wait_for_subscriptions(1, PATIENCE)
        .await
        .expect("the bridge must expose a service request reader");

    let identity = astrs_ros2::service::SampleIdentity::from_guid(request_writer.guid(), 1);
    let request = astrs_ros2::msg::std_srvs::SetBoolRequest { data: true };
    let wire = astrs_ros2::service::encode_with_identity(identity, &request).unwrap();
    request_writer.publish_bytes(wire).await.unwrap();

    let sends = await_sends(&daemon, "request", 1).await;
    let request_id = sends[0]
        .metadata
        .request_id()
        .expect("§9.4: a bridged request carries a `request_id`")
        .to_owned();
    assert_eq!(request_id, identity.to_string());
    let carried = Payload::inline(sends[0].bytes().unwrap().to_vec());
    let decoded = astrs_ros2::msg::std_srvs::SetBoolRequest::from_record_batch(
        carried.batch().expect("a columnar batch"),
    )
    .unwrap();
    assert!(decoded.data, "the request body crossed as columns");

    // Answer it the way a graph node would: same `request_id`, the response
    // type's columnar form.
    let response = astrs_ros2::msg::std_srvs::SetBoolResponse {
        success: true,
        message: "flag set".to_owned(),
    };
    let mut meta = Metadata::new(HlcTimestamp::new(2_000_000_000_000_000_000, 0));
    meta.set_request_id(request_id.clone());
    daemon
        .send_input(
            &bridge_id(),
            &DataId::new("response").unwrap(),
            meta,
            Payload::from_batch(response.to_record_batch().unwrap())
                .unwrap()
                .to_vec(),
        )
        .unwrap();

    let (reply, _) = reply_reader
        .recv_within(PATIENCE)
        .await
        .expect("the client must receive its reply");
    let (reply_identity, body) =
        astrs_ros2_bridge_node::codec::split_identity(&reply, "std_srvs/srv/SetBool_Response")
            .unwrap();
    assert_eq!(
        reply_identity, identity,
        "§9.4: the reply echoes the request's correlation identity"
    );
    let answered: astrs_ros2::msg::std_srvs::SetBoolResponse =
        astrs_cdr::from_bytes_tolerant(&body).unwrap();
    assert_eq!(answered, response);

    daemon.stop(&bridge_id(), StopCause::Requested).unwrap();
    bridge
        .await
        .expect("the bridge task joins")
        .expect("the bridge exits cleanly");
    peer_context.shutdown().await;
    daemon.shutdown();
}

/// The action bridge, end to end, against a hand-rolled ROS 2 action
/// client.
///
/// **Ignored, and not because it is slow.** An action server creates five
/// DDS writers and three readers on one participant, and `astrs-rtps`'s
/// SEDP builtin writers today keep a *flat* `HistoryQos::keep_last(1)`
/// (`crates/astrs-rtps/src/discovery/matching.rs:135` for the writer,
/// `:272` for the reader) where DDS specifies keep-last-1 **per instance**
/// — the builtin topics are keyed by endpoint GUID. So a participant's
/// second and later writers (and readers) are evicted from the transient-
/// local history before a late-joining peer can replay them, and only the
/// most recently created one of each is ever discoverable.
///
/// Reproduced minimally: four publishers created in a burst on one
/// participant, four matching subscriptions on another, three seconds of
/// settling — `writers matched: [(0, 0), (1, 0), (2, 0), (3, 1)]`, and the
/// same for readers. `astrs-rtps`'s own `e2e_*.rs` fixtures never caught it
/// because every one of them is exactly one writer and one reader.
///
/// Every other test in this file passes because a topic or service bridge
/// is exactly one writer and one reader. This one cannot pass until that
/// history is keyed, and it is left here — asserting what the action bridge
/// is supposed to do — so that fixing it is a one-command proof.
#[ignore = "blocked on astrs-rtps SEDP history: crates/astrs-rtps/src/discovery/matching.rs:135"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_action_server_bridge_accepts_a_goal_and_answers_its_parked_result_request() {
    use astrs_ros2::msg::example_interfaces::{
        FibonacciGetResultRequest, FibonacciGetResultResponse, FibonacciGoal, FibonacciResult,
        FibonacciSendGoalRequest, FibonacciSendGoalResponse,
    };
    use astrs_ros2::msg::unique_identifier_msgs::UUID;
    use astrs_ros2::names::TopicKind;
    use astrs_ros2::names::mangle::{ActionEndpoint, action_name};
    use astrs_ros2::service::{SampleIdentity, encode_with_identity};
    use astrs_ros2_bridge_node::action::{parse_goal_id, render_goal_id};

    let (peer_context, peer) = peer_node("action_peer").await;

    let daemon = MockDaemon::start().expect("the mock daemon starts");
    let yaml = "\
compat: humble
action: /fibonacci
message_type: example_interfaces/action/Fibonacci
role: server
";
    let config: Ros2Config = astrs_yaml::from_str(yaml).unwrap();
    let mut spec = NodeSpawnSpec::new(
        daemon.dataflow(),
        bridge_id(),
        0,
        NodeSource::Ros2Bridge {
            config: serde_json::to_string(&config).unwrap(),
        },
    );
    spec = spec.with_output(OutputSpec::new(DataId::new("goal").unwrap()));
    spec = spec.with_input(InputSpec::new(
        DataId::new("result").unwrap(),
        PortRef::from_parts("solver", "out").unwrap(),
    ));
    let (node, events) = daemon.connect_node(spec).expect("the bridge registers");
    let settings = settings_for(&peer_context);

    // Built through the explicit stages rather than `run_with`, so the
    // endpoint counts are asserted before anything is expected to match: a
    // plan that produced no action would otherwise look exactly like an
    // action whose endpoints never discovered their peer.
    let plan = astrs_ros2_bridge_node::plan(&config, node.descriptor()).expect("the plan");
    assert_eq!(plan.actions.len(), 1, "one bridged action");
    let resolved = astrs_ros2_bridge_node::resolve_plan(&plan, &settings).expect("resolution");
    let built = astrs_ros2_bridge_node::Bridge::start(node, events, resolved, &settings)
        .await
        .expect("the bridge starts");
    assert_eq!(
        built.endpoint_counts(),
        (0, 0, 0, 1),
        "no topics, no services, one action"
    );
    let bridge = tokio::spawn(built.pump());

    // A hand-rolled action client: the `send_goal` and `get_result` service
    // halves, mangled exactly as `rcl_action` mangles them.
    let goal_name = FullName::service(action_name("/fibonacci", ActionEndpoint::SendGoal)).unwrap();
    let result_name =
        FullName::service(action_name("/fibonacci", ActionEndpoint::GetResult)).unwrap();

    let goal_writer = RawPublisher::for_kind(
        std::sync::Arc::clone(&peer_context),
        goal_name.clone(),
        TopicKind::Request,
        "example_interfaces::action::dds_::Fibonacci_SendGoal_Request_",
        QosProfile::services_default(),
        Some(peer.fully_qualified_name()),
    )
    .await
    .unwrap();
    let goal_reader = RawSubscription::for_kind(
        std::sync::Arc::clone(&peer_context),
        goal_name,
        TopicKind::Reply,
        "example_interfaces::action::dds_::Fibonacci_SendGoal_Response_",
        QosProfile::services_default(),
        Some(peer.fully_qualified_name()),
    )
    .await
    .unwrap();
    let result_writer = RawPublisher::for_kind(
        std::sync::Arc::clone(&peer_context),
        result_name.clone(),
        TopicKind::Request,
        "example_interfaces::action::dds_::Fibonacci_GetResult_Request_",
        QosProfile::services_default(),
        Some(peer.fully_qualified_name()),
    )
    .await
    .unwrap();
    let result_reader = RawSubscription::for_kind(
        std::sync::Arc::clone(&peer_context),
        result_name,
        TopicKind::Reply,
        "example_interfaces::action::dds_::Fibonacci_GetResult_Response_",
        QosProfile::services_default(),
        Some(peer.fully_qualified_name()),
    )
    .await
    .unwrap();

    goal_writer
        .wait_for_subscriptions(1, PATIENCE)
        .await
        .expect("the bridge must expose a send_goal request reader");
    result_writer
        .wait_for_subscriptions(1, PATIENCE)
        .await
        .expect("the bridge must expose a get_result request reader");

    // ---- send_goal ------------------------------------------------------
    let goal_uuid = [0x5a_u8; 16];
    let goal_identity = SampleIdentity::from_guid(goal_writer.guid(), 1);
    let request = FibonacciSendGoalRequest {
        goal_id: UUID { uuid: goal_uuid },
        goal: FibonacciGoal { order: 7 },
    };
    goal_writer
        .publish_bytes(encode_with_identity(goal_identity, &request).unwrap())
        .await
        .unwrap();

    let sends = await_sends(&daemon, "goal", 1).await;
    assert_eq!(
        sends[0].metadata.goal_id(),
        Some(render_goal_id(&goal_uuid).as_str()),
        "§9.4: a bridged goal carries its `goal_id`"
    );
    assert_eq!(
        sends[0].metadata.goal_status(),
        Some(astrs_wire::GoalStatus::Accepted),
        "§9.4: the FSM starts at Accepted"
    );
    let carried = Payload::inline(sends[0].bytes().unwrap().to_vec());
    let decoded =
        FibonacciSendGoalRequest::from_record_batch(carried.batch().expect("a batch")).unwrap();
    assert_eq!(decoded.goal.order, 7, "the goal body crossed as columns");
    assert_eq!(parse_goal_id(&render_goal_id(&goal_uuid)), Some(goal_uuid));

    let (accept, _) = goal_reader
        .recv_within(PATIENCE)
        .await
        .expect("the client must be told its goal was accepted");
    let (accept_identity, accept_body) =
        astrs_ros2_bridge_node::codec::split_identity(&accept, "send_goal").unwrap();
    assert_eq!(accept_identity, goal_identity);
    let accepted: FibonacciSendGoalResponse = astrs_cdr::from_bytes_tolerant(&accept_body).unwrap();
    assert!(accepted.accepted);

    // ---- get_result, asked before the graph has produced one ------------
    let result_identity = SampleIdentity::from_guid(result_writer.guid(), 1);
    let result_request = FibonacciGetResultRequest {
        goal_id: UUID { uuid: goal_uuid },
    };
    result_writer
        .publish_bytes(encode_with_identity(result_identity, &result_request).unwrap())
        .await
        .unwrap();

    // …and only then answered by the graph. The reply must still arrive:
    // the parked request is what makes `get_result` a long-poll rather than
    // a race the client has to lose.
    let response = FibonacciGetResultResponse {
        status: astrs_wire::GoalStatus::Succeeded.as_i64() as i8,
        result: FibonacciResult {
            sequence: vec![0, 1, 1, 2, 3, 5, 8],
        },
    };
    let mut meta = Metadata::new(HlcTimestamp::new(2_100_000_000_000_000_000, 0));
    meta.set_goal_id(render_goal_id(&goal_uuid));
    meta.set_goal_status(astrs_wire::GoalStatus::Succeeded);
    daemon
        .send_input(
            &bridge_id(),
            &DataId::new("result").unwrap(),
            meta,
            Payload::from_batch(response.to_record_batch().unwrap())
                .unwrap()
                .to_vec(),
        )
        .unwrap();

    let (reply, _) = result_reader
        .recv_within(PATIENCE)
        .await
        .expect("the parked get_result request must be answered once the result arrives");
    let (reply_identity, reply_body) =
        astrs_ros2_bridge_node::codec::split_identity(&reply, "get_result").unwrap();
    assert_eq!(reply_identity, result_identity);
    let answered: FibonacciGetResultResponse = astrs_cdr::from_bytes_tolerant(&reply_body).unwrap();
    assert_eq!(answered, response);

    daemon.stop(&bridge_id(), StopCause::Requested).unwrap();
    bridge
        .await
        .expect("the bridge task joins")
        .expect("the bridge exits cleanly");
    peer_context.shutdown().await;
    daemon.shutdown();
}
