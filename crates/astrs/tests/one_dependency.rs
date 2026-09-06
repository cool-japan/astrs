//! The facade's real contract: **`astrs` alone is enough**.
//!
//! Every other test in this workspace reaches for the crate it is testing.
//! This one deliberately does not: nothing here names `astrs_node_api`,
//! `astrs_data`, `astrs_wire` or any other underlying crate. If a
//! re-export is missing, or a type an application must name is only
//! reachable through a path the facade does not offer, this file stops
//! compiling — which is the only way to find out that "the one dependency
//! an application adds" (blueprint §5.2) is not, in fact, enough.
//!
//! # Coverage
//!
//! The sections below follow blueprint §9.1's own surface table, row for
//! row — Init, Events, Sending, Patterns, Logging, Introspection,
//! Extensions — and then the tooling surfaces the other features front. A
//! facade test that touched each area once would prove the module paths
//! resolve; walking the table proves an application can actually *do* each
//! thing with one dependency, which is a different and more useful claim.
//!
//! Nothing here sleeps. Sends cross a writer task, so assertions wait on
//! the daemon having *seen* the effect, through the harness's own
//! condition-based helpers.
//!
//! The whole file is gated on `node`, the facade's default feature: with
//! it off there is no prelude to write an application against, which is
//! the point of the feature rather than a gap in this test.

#![cfg(feature = "node")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::time::Duration;

use astrs::node_api::TestHarness;
use astrs::prelude::*;

/// How long an assertion waits for the daemon to observe an effect.
///
/// Generous on purpose: it is a *failure* bound, not a synchronization
/// device — every wait below returns the instant its condition holds.
const WAIT: Duration = Duration::from_secs(5);

/// A message type declared entirely through the facade: the derive comes
/// from `astrs::prelude`, the trait it implements from the same name in the
/// type namespace, and the URN grammar from blueprint §24.3.
#[derive(AstrsMessage, Debug, Clone, PartialEq)]
#[astrs(urn = "std/test/v1/Reading")]
struct Reading {
    values: Vec<f64>,
    labels: Vec<u32>,
}

impl Reading {
    fn sample() -> Self {
        Self {
            values: vec![1.5, 2.5],
            labels: vec![7, 9],
        }
    }
}

/// The output name [`TestHarness::start`] declares, as a [`DataId`].
fn default_output() -> DataId {
    DataId::new(TestHarness::DEFAULT_OUTPUT).expect("the harness's own name is valid")
}

// ===========================================================================
// §9.1 — the names the flagship sample says out loud
// ===========================================================================

/// The zero-copy views §9.1's loop annotates its very first binding with.
///
/// Naming the types *through the prelude* is the whole test: before they were
/// re-exported, `let img: ImageView = data.view()?;` needed a second `use`
/// naming `astrs-data`, which is precisely the shape this file exists to
/// catch. `TensorView` rides along because a prelude that carried the image
/// case and not the general one would send the next reader back to the
/// underlying crate anyway.
///
/// The signature is the assertion; the body only has to be reachable.
fn _the_views_are_named_through_the_prelude(
    image: Option<ImageView>,
    tensor: Option<TensorView<f32>>,
) {
    let _ = (image.is_none(), tensor.is_none());
}

#[test]
fn the_view_types_are_reachable_from_the_prelude() {
    _the_views_are_named_through_the_prelude(None, None);
    // `Payload::view` is what turns them into values, and it is generic over
    // the same `FromPayload` the prelude re-exports.
    fn _accepts<T: FromPayload>(payload: &Payload) -> Result<T> {
        payload.view::<T>()
    }
}

#[cfg(feature = "arrow-interop")]
#[test]
fn the_arrow_send_surface_is_reachable_from_the_facade() {
    // §9.1 lists `send_arrow(ArrayRef) [arrow-interop]` on the untyped send
    // row. Reaching it — and the error it reports — must not need a second
    // dependency any more than the rest of the row does.
    let mut harness = Node::init_testing().expect("the in-process daemon starts");
    let mut out = harness
        .node
        .raw_output(TestHarness::DEFAULT_OUTPUT)
        .expect("the harness declares this output");

    // The bridge, reached only through the facade: an AstRS column becomes an
    // arrow-rs batch, and that batch goes straight back out.
    let column = astrs::data::array::IntoArrayRef::into_array_ref(
        astrs::data::array::Float64Array::from_values([2.5, 3.5]),
    );
    let batch = astrs::data::RecordBatch::from_payload(column);
    let arrow_batch =
        astrs::data::interop::to_arrow_record_batch(&batch).expect("maps onto arrow-rs");
    out.send_arrow(&arrow_batch, harness.node.metadata())
        .expect("the arrow batch publishes");

    // The error type is part of the reachable surface too.
    fn _accepts(_: ArrowSendError) {}
    let _: ArrowSendResult<()> = Ok(());

    let sends = harness
        .daemon
        .wait_for_sends(harness.node.id(), &default_output(), 1, WAIT)
        .expect("the daemon observes the send");
    assert!(sends[0].bytes().is_some_and(|bytes| !bytes.is_empty()));
    harness.shutdown();
}

#[cfg(feature = "operator")]
#[test]
fn the_registry_macro_and_its_table_are_reachable_from_the_facade_root() {
    // Blueprint §9.3 writes `astrs::register_operator!(MyOp)` — at the root,
    // not at `astrs::operator_api`. `astrs new operator`'s scaffold writes
    // the same line. Both forms are checked here, plus the table they feed.
    #[derive(Default)]
    struct Noop;

    impl Operator for Noop {
        fn on_event(&mut self, _event: &OpEvent, _out: &mut OpOutput) -> OpResult<Status> {
            Ok(Status::Continue)
        }
    }

    let root: astrs::OperatorRegistry =
        astrs::OperatorRegistry::from_entries([astrs::register_operator!(Noop)])
            .expect("one operator, one name");
    assert!(root.contains("Noop"));

    // The same two names again, this time as the prelude glob imported them.
    let via_prelude: OperatorRegistry =
        OperatorRegistry::from_entries([register_operator!(Noop)]).expect("one operator, one name");
    assert_eq!(via_prelude.len(), 1);

    // And the attribute form, which must produce an interchangeable entry.
    #[astrs::operator]
    #[derive(Default)]
    struct CropOperator;

    impl Operator for CropOperator {
        fn on_event(&mut self, _event: &OpEvent, _out: &mut OpOutput) -> OpResult<Status> {
            Ok(Status::Continue)
        }
    }

    let via_attribute = OperatorRegistry::from_entries([CropOperator::operator_entry()])
        .expect("one operator, one name");
    assert!(
        via_attribute.contains("crop_operator"),
        "the attribute's default name is the struct's, snake-cased"
    );
}

// ===========================================================================
// §9.1 — Init
// ===========================================================================

#[test]
fn the_testing_constructor_starts_a_whole_node() {
    let mut harness = Node::init_testing().expect("the in-process daemon starts");
    assert_eq!(harness.node.id().as_str(), TestHarness::DEFAULT_NODE);

    // Subscription is part of the handshake and completes asynchronously,
    // so the assertion waits on the daemon rather than racing it.
    let daemon = harness.daemon.clone();
    let node = harness.node.id().clone();
    daemon
        .wait_for(WAIT, |_| daemon.is_subscribed(&node))
        .expect("the node subscribes");
    harness.shutdown();
}

#[test]
fn the_builder_is_reachable_and_composable() {
    // The builder is configured, then *not* connected: `connect()` needs a
    // daemon endpoint, and the point here is that every configuration
    // method an application calls is on the facade's own `NodeBuilder`.
    let builder = Node::builder()
        .node_id("assembled")
        .expect("a valid node id")
        .input("frames")
        .expect("a valid input name")
        .output("detections")
        .expect("a valid output name")
        .label("facade-test");
    // `provisional_spec` is the *identity* half — the ports are settled
    // against the daemon at `connect()`, which is why it carries only
    // what the builder can know on its own.
    let spec = builder
        .provisional_spec()
        .expect("a builder with an id has a spec");
    assert_eq!(spec.node.as_str(), "assembled");

    // A malformed name is refused by the same surface, through the same
    // error type.
    assert!(
        Node::builder().node_id("Not A Valid Id").is_err(),
        "the builder validates identifiers rather than deferring to the daemon"
    );
}

#[test]
fn the_dynamic_attach_path_is_reachable() {
    // `init_from_node_id` is the `path: dynamic` entry point (§8.3). With
    // no daemon to attach to it must *fail*, and failing through the
    // facade's own error type is exactly what this checks — the surface is
    // reachable and typed, without needing a cluster.
    let err = Node::init_from_node_id("not-attached-to-anything")
        .expect_err("no daemon is listening in a unit test");
    let _: &dyn std::error::Error = &err;
    assert!(!err.to_string().is_empty());
}

// ===========================================================================
// §9.1 — Events and Sending
// ===========================================================================

#[test]
fn a_node_written_against_the_facade_alone_runs() {
    let mut harness = Node::init_testing().expect("the in-process daemon starts");
    let mut out = harness
        .node
        .raw_output(TestHarness::DEFAULT_OUTPUT)
        .expect("the harness declares this output");

    harness.feed(b"frame-0".to_vec()).expect("delivery");

    match harness.next_event().expect("an event arrives") {
        Event::Input { id, data, meta } => {
            assert_eq!(id.as_str(), TestHarness::DEFAULT_INPUT);
            assert_eq!(data.bytes(), b"frame-0");
            out.send_bytes(b"echoed", meta.follow()).expect("send");
        }
        other => panic!("expected an input event, got {other:?}"),
    }

    // A send crosses a writer task, so the assertion waits on the daemon
    // having *seen* it rather than on a sleep — the harness's own rule.
    let sends = harness
        .daemon
        .wait_for_sends(harness.node.id(), &default_output(), 1, WAIT)
        .expect("the echo reaches the daemon");
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].bytes(), Some(b"echoed".as_slice()));
    harness.shutdown();
}

#[test]
fn a_typed_output_publishes_through_the_facade() {
    let mut harness = Node::init_testing().expect("harness");
    let mut out = harness
        .node
        .output::<Reading>(TestHarness::DEFAULT_OUTPUT)
        .expect("a typed handle on the declared output");

    out.send(Reading::sample(), harness.node.metadata())
        .expect("send");

    let sends = harness
        .daemon
        .wait_for_sends(harness.node.id(), &default_output(), 1, WAIT)
        .expect("the daemon observes the send");
    assert_eq!(sends.len(), 1);
    assert!(
        sends[0].bytes().is_some_and(|bytes| !bytes.is_empty()),
        "a typed send carries an Arrow IPC payload"
    );
    harness.shutdown();
}

#[test]
fn the_stop_event_ends_the_loop_through_the_facade_types() {
    let mut harness = Node::init_testing().expect("harness");
    harness
        .daemon
        .stop(harness.node.id(), StopCause::Requested)
        .expect("stop");

    let mut saw_stop = false;
    while let Ok(event) = harness.next_event() {
        if let Event::Stop(cause) = event {
            assert_eq!(cause, StopCause::Requested);
            saw_stop = true;
            break;
        }
    }
    assert!(saw_stop, "the loop must see its own Stop");
    harness.shutdown();
}

#[test]
fn the_allocator_hands_back_a_writable_sample() {
    let mut harness = Node::init_testing().expect("harness");
    let mut out = harness
        .node
        .raw_output(TestHarness::DEFAULT_OUTPUT)
        .expect("output");

    // §9.1's `allocate(len) -> SampleMut`: the zero-copy path, which falls
    // back to an ordinary buffer until a route is upgraded, so a node
    // writes the same code either way.
    let mut sample = out.allocate(8).expect("an allocation");
    assert_eq!(sample.len(), 8);
    sample.as_mut_slice().copy_from_slice(b"01234567");
    assert_eq!(sample.as_slice(), b"01234567");
    sample
        .send(harness.node.metadata())
        .expect("the sample publishes");

    let sends = harness
        .daemon
        .wait_for_sends(harness.node.id(), &default_output(), 1, WAIT)
        .expect("the daemon observes the sample");
    assert_eq!(sends[0].bytes(), Some(b"01234567".as_slice()));
    harness.shutdown();
}

#[test]
fn a_wired_pair_moves_a_message_between_two_nodes() {
    let (mut producer, mut consumer) =
        TestHarness::pair("camera", "frames", "detector", "frames").expect("a wired pair starts");
    let frames_id = DataId::new("frames").expect("valid id");

    let mut frames = producer
        .node
        .raw_output("frames")
        .expect("the producer declares it");
    let metadata = producer.node.metadata();
    frames
        .send_bytes(b"frame-0", metadata.clone())
        .expect("send");
    producer
        .daemon
        .wait_for_sends(producer.node.id(), &frames_id, 1, WAIT)
        .expect("the daemon sees the frame");

    // The mock daemon records rather than routes, so the delivery is
    // driven explicitly — itself a facade-reachability check on
    // `MockDaemon`, which an integration test of a real node uses the same
    // way.
    consumer
        .daemon
        .send_input(
            consumer.node.id(),
            &frames_id,
            metadata,
            b"frame-0".to_vec(),
        )
        .expect("delivery");

    match consumer.next_event().expect("an event arrives") {
        Event::Input { id, data, .. } => {
            assert_eq!(id.as_str(), "frames");
            assert_eq!(data.bytes(), b"frame-0");
        }
        other => panic!("expected an input, got {other:?}"),
    }

    producer.shutdown();
    consumer.shutdown();
}

// ===========================================================================
// §9.1 / §9.4 — Patterns: services, actions, streams
// ===========================================================================

#[test]
fn a_service_request_and_its_response_correlate_through_the_facade() {
    let mut harness = Node::init_testing().expect("harness");
    let mut out = harness
        .node
        .raw_output(TestHarness::DEFAULT_OUTPUT)
        .expect("output");

    let request: RequestId = harness
        .node
        .service_request_bytes(&mut out, b"please")
        .expect("the request publishes");
    let sends = harness
        .daemon
        .wait_for_sends(harness.node.id(), &default_output(), 1, WAIT)
        .expect("the daemon observes the request");
    assert_eq!(
        sends[0].metadata.request_id(),
        Some(request.as_str()),
        "the correlation id rides the metadata"
    );

    // A server answers from the *request's* metadata, so it cannot reply
    // with the wrong id.
    harness
        .node
        .service_response_bytes(&mut out, &sends[0].metadata, b"here")
        .expect("the response publishes");
    let sends = harness
        .daemon
        .wait_for_sends(harness.node.id(), &default_output(), 2, WAIT)
        .expect("the daemon observes the response");
    assert_eq!(sends[1].metadata.request_id(), Some(request.as_str()));

    // And the client side recognises it, through the facade's own
    // `ServiceResponse`.
    let response = ServiceResponse::from_event(Event::Input {
        id: default_output(),
        data: Payload::inline(b"here".to_vec()),
        meta: sends[1].metadata.clone(),
    })
    .expect("a correlated event is a response");
    assert!(response.matches(&request));
    assert!(
        !response.matches(&RequestId::generate()),
        "a response must not match a request it does not answer"
    );
    harness.shutdown();
}

#[test]
fn an_action_goal_walks_its_status_fsm_through_the_facade() {
    let mut harness = Node::init_testing().expect("harness");
    let mut out = harness
        .node
        .raw_output(TestHarness::DEFAULT_OUTPUT)
        .expect("output");

    let goal: GoalId = harness
        .node
        .goal_bytes(&mut out, b"go-there")
        .expect("the goal publishes");
    for status in [GoalStatus::Executing, GoalStatus::Succeeded] {
        harness
            .node
            .goal_signal(&mut out, &goal, status)
            .expect("a status update publishes");
    }

    let sends = harness
        .daemon
        .wait_for_sends(harness.node.id(), &default_output(), 3, WAIT)
        .expect("the daemon observes the whole FSM");
    assert_eq!(sends[0].metadata.goal_status(), Some(GoalStatus::Accepted));
    assert_eq!(sends[1].metadata.goal_status(), Some(GoalStatus::Executing));
    assert_eq!(sends[2].metadata.goal_status(), Some(GoalStatus::Succeeded));
    for send in &sends {
        assert_eq!(send.metadata.goal_id(), Some(goal.as_str()));
    }

    // The client-side tracker is part of the same surface.
    let mut tracker = GoalTracker::new();
    tracker.track(goal.clone());
    assert_eq!(tracker.in_flight(), 1);
    assert_eq!(tracker.goals(), vec![goal.clone()]);

    // Replaying the same statuses through the client-side tracker walks it
    // to a terminal state, which is what an action client's loop does.
    for send in &sends {
        let outcome = ActionOutcome::from_event(Event::Input {
            id: default_output(),
            data: Payload::empty(),
            meta: send.metadata.clone(),
        })
        .expect("a goal-stamped event is an outcome");
        tracker.observe(&outcome).expect("a tracked goal");
    }
    assert_eq!(tracker.status(&goal), Some(GoalStatus::Succeeded));
    assert_eq!(tracker.prune_terminal(), 1, "the finished goal is retired");
    assert_eq!(tracker.in_flight(), 0);
    harness.shutdown();
}

#[test]
fn a_stream_segment_numbers_itself_through_the_facade() {
    let mut harness = Node::init_testing().expect("harness");
    let mut out = harness
        .node
        .raw_output(TestHarness::DEFAULT_OUTPUT)
        .expect("output");
    let mut writer = StreamWriter::with_session("facade-session");

    let chunks = harness
        .node
        .stream_segment(&mut out, &mut writer, b"0123456789", 4)
        .expect("the segment publishes");
    assert_eq!(chunks, 3, "10 bytes at 4 per chunk");

    let sends = harness
        .daemon
        .wait_for_sends(harness.node.id(), &default_output(), 3, WAIT)
        .expect("the daemon observes every chunk");
    // The writer numbers the run: the same session throughout, ascending
    // sequence numbers, and only the last chunk carries `fin`.
    let mut seen_fin = 0;
    for (index, send) in sends.iter().enumerate() {
        let chunk = ChunkRef::from_metadata(&send.metadata).expect("a chunk reference");
        assert_eq!(chunk.session, "facade-session");
        assert_eq!(chunk.seq, i64::try_from(index).expect("three chunks fit"));
        if chunk.fin {
            seen_fin += 1;
        }
    }
    assert_eq!(seen_fin, 1, "exactly the last chunk ends the segment");

    // The reading half: an assembler reconstructs the payload the writer
    // split, and reports the segment only once the final chunk lands.
    let mut assembler = StreamAssembler::new();
    let mut completed: Option<StreamSegment> = None;
    for send in &sends {
        let event = Event::Input {
            id: default_output(),
            data: Payload::inline(send.bytes().unwrap_or_default().to_vec()),
            meta: send.metadata.clone(),
        };
        match assembler
            .accept_event(event)
            .expect("a chunk is acceptable")
        {
            Ok(Some(segment)) => completed = Some(segment),
            Ok(None) => {}
            Err(event) => panic!("a chunk was not recognised: {event:?}"),
        }
    }
    let segment = completed.expect("the final chunk closes the segment");
    assert_eq!(segment.bytes.as_slice(), b"0123456789");
    assert_eq!(segment.session, "facade-session");
    assert_eq!(segment.chunks, 3);
    assert!(assembler.open_segments().is_empty());
    harness.shutdown();
}

// ===========================================================================
// §9.1 — Logging
// ===========================================================================

#[test]
fn every_log_level_reaches_the_daemon_through_the_facade() {
    let harness = Node::init_testing().expect("harness");
    harness.node.log_error("an error");
    harness.node.log_warn("a warning");
    harness.node.log_info("some information");
    harness.node.log_debug("a detail");
    harness.node.log_trace("a trace");

    let mut fields = BTreeMap::new();
    fields.insert("frame".to_string(), "42".to_string());
    harness
        .node
        .log_with_fields(astrs::wire::LogLevel::Info, "structured", fields);

    let daemon = harness.daemon.clone();
    daemon
        .wait_for(WAIT, |_| daemon.logs().len() >= 6)
        .expect("six records reach the daemon");

    let logs = daemon.logs();
    let messages: Vec<&str> = logs.iter().map(|record| record.message.as_str()).collect();
    for expected in [
        "an error",
        "a warning",
        "some information",
        "a detail",
        "a trace",
        "structured",
    ] {
        assert!(
            messages.contains(&expected),
            "{expected} missing: {messages:?}"
        );
    }
}

// ===========================================================================
// §9.1 — Introspection
// ===========================================================================

#[test]
fn the_node_reports_its_own_identity() {
    let harness = Node::init_testing().expect("harness");
    assert_eq!(harness.node.id().as_str(), TestHarness::DEFAULT_NODE);
    assert!(!harness.node.is_restart());
    assert_eq!(harness.node.restart_count(), 0);
    let _: DataflowId = harness.node.dataflow_id();

    // The hybrid logical clock is reachable, and it advances.
    let first = harness.node.hlc_now();
    let second = harness.node.hlc_now();
    assert!(second >= first, "the hybrid logical clock is monotonic");

    // So is the descriptor the daemon handed over.
    let descriptor = harness.node.descriptor();
    assert_eq!(descriptor.node.as_str(), TestHarness::DEFAULT_NODE);
    assert_eq!(descriptor.outputs.len(), 1);
}

// ===========================================================================
// §9.1 — Extensions
// ===========================================================================

#[test]
fn the_extension_table_round_trips_through_the_facade() {
    let harness = Node::init_testing().expect("harness");
    // `ExtensionKey` lives in the wire crate, so reaching it is itself a
    // facade check.
    let key = astrs::wire::ExtensionKey::user("facade-scratch").expect("a valid key");

    harness
        .node
        .ext_store(&key, b"stored".to_vec())
        .expect("store");
    assert_eq!(
        harness.node.ext_load(&key).expect("load"),
        Some(b"stored".to_vec())
    );
    assert_eq!(harness.daemon.extension(&key), Some(b"stored".to_vec()));

    harness.node.ext_drop(&key).expect("drop");
    assert_eq!(harness.node.ext_load(&key).expect("load"), None);
}

// ===========================================================================
// The build itself
// ===========================================================================

#[test]
fn build_info_describes_this_very_build() {
    let line = astrs::build_info::summary();
    assert!(line.starts_with("astrs "), "{line}");
    assert!(line.contains(astrs::VERSION), "{line}");
    assert_eq!(astrs::build_info::is_enabled("node"), Some(true));
    assert_eq!(
        astrs::build_info::is_enabled("verify"),
        Some(cfg!(feature = "verify"))
    );
    assert_eq!(astrs::build_info::can_prove(), cfg!(feature = "verify"));

    let report = astrs::build_info::report();
    assert!(report.contains("node"), "{report}");
    assert!(report.contains("astrs-node-api"), "{report}");
}

// ===========================================================================
// The tooling surfaces, each reached only through the facade
// ===========================================================================

#[cfg(feature = "data")]
#[test]
fn a_typed_message_round_trips_through_the_facade() {
    // The derive's own contract, reachable without naming `astrs-data`.
    assert_eq!(<Reading as AstrsMessage>::URN, "std/test/v1/Reading");

    let reading = Reading::sample();
    let batch = reading.to_record_batch().expect("encodes");
    let back = Reading::from_record_batch(&batch).expect("decodes");
    assert_eq!(back, reading);

    // The schema the manifest's `output_types:` is checked against.
    let schema = <Reading as AstrsMessage>::schema();
    assert_eq!(schema.fields().len(), 1);
}

#[cfg(feature = "graph")]
#[test]
fn a_manifest_becomes_a_graph_without_naming_either_crate() {
    let manifest = astrs::manifest::Manifest::from_yaml_str(
        "
nodes:
  - id: camera
    path: ./camera
    inputs:
      tick:
        source: astrs/timer/hz/10
        queue_size: 1
    outputs: [frames]
  - id: detector
    path: ./detector
    inputs:
      frames:
        source: camera/frames
        queue_size: 2
",
    )
    .expect("parses");
    manifest.validate().expect("validates");
    let (graph, construction) =
        astrs::graph::DataflowGraph::from_manifest(&manifest).expect("builds");
    assert!(construction.is_empty(), "{construction:?}");
    assert_eq!(graph.node_count(), 2);
    assert_eq!(graph.edge_count(), 2);
    assert!(graph.diagnostics().is_empty());

    // Visualization and placement are part of the same surface.
    let mermaid = astrs::graph::to_mermaid(&graph);
    assert!(mermaid.contains("camera"), "{mermaid}");
    let dot = astrs::graph::to_dot(&graph);
    assert!(dot.contains("digraph"), "{dot}");
    let plan = astrs::graph::plan_placement(&graph);
    assert!(!plan.machines.is_empty());
}

#[cfg(feature = "verify")]
#[test]
fn a_graph_is_proved_through_the_facade() {
    let manifest = astrs::manifest::Manifest::from_yaml_str(
        "
nodes:
  - id: planner
    path: ./planner
    inputs: { pose: localizer/pose }
    outputs: [plan]
  - id: localizer
    path: ./localizer
    inputs: { plan: planner/plan }
    outputs: [pose]
",
    )
    .expect("parses");
    manifest.validate().expect("validates");
    let (graph, _) = astrs::graph::DataflowGraph::from_manifest(&manifest).expect("builds");

    let report = astrs::verify::prove(&graph, &astrs::verify::ProveOptions::default())
        .expect("the model builds");
    assert!(report.has_violations(), "this graph deadlocks");

    let rendered = astrs::verify::render_human(&report, &astrs::verify::RenderOptions::default());
    assert!(rendered.contains("can never fire"), "{rendered}");

    // A profile is part of the same surface.
    let profile = astrs::verify::Profile::from_yaml_str("nodes:\n  planner:\n    wcet: 0.001\n")
        .expect("a valid profile");
    assert!(profile.validate_against(&graph).is_empty());
}

#[cfg(feature = "operator")]
#[test]
fn an_operator_is_written_and_hosted_through_the_facade() {
    use astrs::operator_api::{OpEvent, OpOutput, OpResult, Operator, OperatorRegistry, Status};

    #[derive(Default)]
    struct Doubler {
        seen: usize,
    }

    impl Operator for Doubler {
        fn on_event(&mut self, event: &OpEvent, out: &mut OpOutput) -> OpResult<Status> {
            match event {
                OpEvent::Input {
                    id,
                    metadata,
                    payload,
                    ..
                } => {
                    self.seen += 1;
                    let mut doubled = payload.clone();
                    doubled.extend_from_slice(payload);
                    out.send_bytes(id.as_str(), metadata.follow(), doubled)?;
                    Ok(Status::Continue)
                }
                OpEvent::Stop { .. } => Ok(Status::Finished),
                _ => Ok(Status::Continue),
            }
        }
    }

    let mut registry = OperatorRegistry::new();
    registry
        .register("Doubler", Box::new(|| Box::<Doubler>::default()))
        .expect("one name");
    let mut operator = registry.build("Doubler").expect("registered");

    let mut out = OpOutput::new();
    let event = OpEvent::Input {
        id: DataId::new("in").expect("valid id"),
        source: "camera/frames".parse().expect("valid port"),
        metadata: Metadata::new(astrs::time::HlcTimestamp::EPOCH),
        payload: vec![1, 2, 3],
    };
    assert_eq!(
        operator.on_event(&event, &mut out).expect("no failure"),
        Status::Continue
    );
    let sent = out.drain();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].payload(), &[1, 2, 3, 1, 2, 3]);

    assert_eq!(
        operator
            .on_event(
                &OpEvent::Stop {
                    cause: StopCause::Requested,
                    grace: None,
                },
                &mut out
            )
            .expect("no failure"),
        Status::Finished
    );
}

#[cfg(feature = "recording")]
#[test]
fn the_recording_surface_is_reachable() {
    // Naming the error type is enough to prove the module is re-exported;
    // writing a container belongs to that crate's own tests.
    fn _accepts(_: astrs::recording::RecordingError) {}
    assert!(std::mem::size_of::<astrs::recording::RecordingError>() > 0);
}

#[cfg(feature = "telemetry")]
#[test]
fn the_telemetry_surface_is_reachable() {
    // The configuration type is the entry point an application names; the
    // subscriber itself is process-global, so installing one here would
    // fight every other test in the binary.
    let config = astrs::telemetry::TelemetryConfig::default();
    assert!(matches!(
        config.format,
        astrs::telemetry::LogFormat::Human | astrs::telemetry::LogFormat::Json
    ));
    fn _accepts(_: astrs::telemetry::TelemetryError) {}
}
