//! Property tests over the wire protocol (blueprint §20.2).
//!
//! Three properties, asserted over generated values rather than hand-picked
//! ones:
//!
//! 1. **Round trip.** Every message of every family survives
//!    `encode → decode` unchanged, and survives a full frame round trip on
//!    both the UDS and network policies.
//! 2. **No panic on hostile input.** Arbitrary bytes, and arbitrary
//!    *corruptions* of valid frames, produce a typed [`WireError`] or a
//!    plausible message — never a panic, never an unbounded allocation.
//! 3. **Trailing bytes are fatal.** A payload with anything appended is
//!    rejected, which is the property blueprint §7.1 calls out by name as
//!    dora's hard-won lesson.
//!
//! Plus the algebraic properties the handshake rests on: feature negotiation
//! is an intersection, limit negotiation is a minimum, and neither can grant
//! more than both ends offered.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_time::HlcTimestamp;
use astrs_wire::{
    Acceptor, AnyMessage, AuthToken, Compression, ControlReply, ControlRequest, CoordinatorEvent,
    DaemonEvent, DataId, DataflowId, DurationMs, ErrorCode, FeatureFlags, FrameBuffer, FrameFlags,
    FrameKind, FrameLimits, FrameReader, FrameWriter, Hello, LogLevel, Metadata, NegotiatedLimits,
    NodeEvent, NodeId, NodeRequest, OutputPayload, ParamScope, Parameter, PeerEvent, PortRef,
    RouteCloseReason, RouteId, SessionAssignment, SessionId, StopCause, SubscriptionId, WireError,
    WireMessage, accept_welcome, decode_frame, negotiate,
};
use proptest::prelude::*;

/// Strategies for the protocol's building blocks and families.
mod arb {
    use super::*;

    /// A valid identifier name.
    pub fn name() -> impl Strategy<Value = String> {
        proptest::string::string_regex("[a-z][a-z0-9_.-]{0,15}").expect("a valid pattern")
    }

    /// A node id.
    pub fn node_id() -> impl Strategy<Value = NodeId> {
        name().prop_map(|text| NodeId::new(text).expect("the pattern only makes valid names"))
    }

    /// A data (port) id.
    pub fn data_id() -> impl Strategy<Value = DataId> {
        name().prop_map(|text| DataId::new(text).expect("the pattern only makes valid names"))
    }

    /// A `node/port` reference.
    pub fn port_ref() -> impl Strategy<Value = PortRef> {
        (node_id(), data_id()).prop_map(|(node, port)| PortRef::new(node, port))
    }

    /// A dataflow id.
    pub fn dataflow_id() -> impl Strategy<Value = DataflowId> {
        any::<u128>().prop_map(DataflowId::from_u128)
    }

    /// An HLC timestamp.
    pub fn timestamp() -> impl Strategy<Value = HlcTimestamp> {
        (any::<u64>(), any::<u32>())
            .prop_map(|(physical, logical)| HlcTimestamp::new(physical, logical))
    }

    /// A parameter value.
    ///
    /// Floats are restricted to the finite range: `NaN` survives the wire
    /// bit-for-bit but compares unequal to itself, which the dedicated
    /// `bitwise_eq` unit tests cover instead.
    pub fn parameter() -> impl Strategy<Value = Parameter> {
        prop_oneof![
            any::<bool>().prop_map(Parameter::Bool),
            any::<i64>().prop_map(Parameter::Integer),
            (-1e12f64..1e12f64).prop_map(Parameter::Float),
            name().prop_map(Parameter::String),
            proptest::collection::vec(any::<i64>(), 0..4).prop_map(Parameter::ListInt),
            proptest::collection::vec(name(), 0..4).prop_map(Parameter::ListString),
        ]
    }

    /// Metadata with a handful of parameters.
    pub fn metadata() -> impl Strategy<Value = Metadata> {
        (
            timestamp(),
            proptest::collection::vec((name(), parameter()), 0..4),
        )
            .prop_map(|(stamp, entries)| {
                let mut metadata = Metadata::new(stamp);
                for (key, value) in entries {
                    let _ = metadata.insert(&key, value);
                }
                metadata
            })
    }

    /// A payload of a plausible size.
    pub fn payload() -> impl Strategy<Value = Vec<u8>> {
        proptest::collection::vec(any::<u8>(), 0..512)
    }

    /// An optional duration.
    pub fn duration() -> impl Strategy<Value = Option<DurationMs>> {
        proptest::option::of(any::<u64>().prop_map(DurationMs::new))
    }

    /// A route-close reason.
    pub fn close_reason() -> impl Strategy<Value = RouteCloseReason> {
        prop_oneof![
            Just(RouteCloseReason::ProducerFinished),
            any::<u64>().prop_map(|generation| RouteCloseReason::ProducerCrashed { generation }),
            Just(RouteCloseReason::ConsumerGone),
            Just(RouteCloseReason::DataflowStopped),
            Just(RouteCloseReason::DaemonShutdown),
            name().prop_map(|message| RouteCloseReason::Error { message }),
        ]
    }

    /// A parameter scope.
    pub fn param_scope() -> impl Strategy<Value = ParamScope> {
        prop_oneof![
            Just(ParamScope::Global),
            dataflow_id().prop_map(ParamScope::dataflow_scope),
            (dataflow_id(), node_id())
                .prop_map(|(dataflow, node)| ParamScope::node(dataflow, node)),
        ]
    }

    /// A CLI → coordinator request.
    pub fn control_request() -> impl Strategy<Value = ControlRequest> {
        prop_oneof![
            any::<bool>().prop_map(|all| ControlRequest::List { all }),
            (dataflow_id(), duration())
                .prop_map(|(dataflow, grace)| ControlRequest::Stop { dataflow, grace }),
            (name(), duration())
                .prop_map(|(name, grace)| ControlRequest::StopByName { name, grace }),
            (dataflow_id(), any::<bool>()).prop_map(|(dataflow, include_nodes)| {
                ControlRequest::Info {
                    dataflow,
                    include_nodes,
                }
            }),
            (dataflow_id(), port_ref(), metadata(), payload()).prop_map(
                |(dataflow, port, metadata, payload)| ControlRequest::TopicPublish {
                    dataflow,
                    port,
                    metadata,
                    payload,
                }
            ),
            (param_scope(), name(), parameter(), any::<bool>()).prop_map(
                |(scope, key, value, create_only)| ControlRequest::SetParam {
                    scope,
                    key: astrs_wire::ParamKey::new(key).expect("valid"),
                    value,
                    create_only,
                }
            ),
            any::<u64>().prop_map(|id| ControlRequest::TopicUnsubscribe {
                subscription: SubscriptionId::new(id)
            }),
            (dataflow_id(), node_id())
                .prop_map(|(dataflow, node)| ControlRequest::RestartNode { dataflow, node }),
        ]
    }

    /// A coordinator → CLI reply.
    pub fn control_reply() -> impl Strategy<Value = ControlReply> {
        prop_oneof![
            Just(ControlReply::Ok),
            (name(), proptest::collection::vec(name(), 0..3)).prop_map(|(message, context)| {
                ControlReply::Error {
                    code: ErrorCode::Internal,
                    message,
                    context,
                }
            }),
            (dataflow_id(), proptest::option::of(name()))
                .prop_map(|(dataflow, name)| ControlReply::Started { dataflow, name }),
            (param_scope(), name(), proptest::option::of(parameter())).prop_map(
                |(scope, key, value)| ControlReply::ParamValue {
                    key: astrs_wire::ParamKey::new(key).expect("valid"),
                    value,
                    scope,
                }
            ),
        ]
    }

    /// A coordinator → daemon event.
    pub fn coordinator_event() -> impl Strategy<Value = CoordinatorEvent> {
        prop_oneof![
            (any::<u64>(), timestamp())
                .prop_map(|(seq, sent_at)| CoordinatorEvent::Heartbeat { seq, sent_at }),
            (dataflow_id(), duration()).prop_map(|(dataflow, grace)| {
                CoordinatorEvent::StopDataflow {
                    dataflow,
                    grace,
                    cause: StopCause::Requested,
                }
            }),
            (dataflow_id(), node_id(), any::<u64>()).prop_map(|(dataflow, node, generation)| {
                CoordinatorEvent::RestartNode {
                    dataflow,
                    node,
                    generation,
                }
            }),
            (param_scope(), name(), parameter()).prop_map(|(scope, key, value)| {
                CoordinatorEvent::SetParam {
                    scope,
                    key: astrs_wire::ParamKey::new(key).expect("valid"),
                    value,
                }
            }),
            duration().prop_map(|grace| CoordinatorEvent::Destroy { grace }),
        ]
    }

    /// A daemon → coordinator event.
    pub fn daemon_event() -> impl Strategy<Value = DaemonEvent> {
        prop_oneof![
            (any::<u64>(), any::<u32>())
                .prop_map(|(seq, applied)| DaemonEvent::StateCatchUpAck { seq, applied }),
            (any::<bool>(), name())
                .prop_map(|(graceful, message)| DaemonEvent::Exit { graceful, message }),
            (dataflow_id(), proptest::collection::vec(node_id(), 0..4))
                .prop_map(|(dataflow, nodes)| DaemonEvent::AllNodesReady { dataflow, nodes }),
        ]
    }

    /// A node → daemon request.
    pub fn node_request() -> impl Strategy<Value = NodeRequest> {
        prop_oneof![
            (data_id(), metadata(), payload()).prop_map(|(output, metadata, bytes)| {
                NodeRequest::SendMessage {
                    output,
                    metadata,
                    payload: OutputPayload::inline(bytes),
                }
            }),
            data_id().prop_map(|output| NodeRequest::OutputDone { output }),
            proptest::collection::vec(data_id(), 0..4)
                .prop_map(|outputs| NodeRequest::CloseOutputs { outputs }),
            Just(NodeRequest::EventStreamDropped),
            (duration(), any::<u32>())
                .prop_map(|(timeout, max_batch)| NodeRequest::NextEvent { timeout, max_batch }),
        ]
    }

    /// A daemon → node event.
    pub fn node_event() -> impl Strategy<Value = NodeEvent> {
        prop_oneof![
            (data_id(), port_ref(), metadata(), payload()).prop_map(
                |(id, source, metadata, payload)| NodeEvent::Input {
                    id,
                    source,
                    metadata,
                    payload,
                }
            ),
            (data_id(), port_ref(), close_reason())
                .prop_map(|(id, source, reason)| { NodeEvent::InputClosed { id, source, reason } }),
            Just(NodeEvent::AllInputsClosed),
            duration().prop_map(|grace| NodeEvent::Stop {
                cause: StopCause::Requested,
                grace
            }),
            (node_id(), any::<u64>())
                .prop_map(|(peer, generation)| NodeEvent::Restarted { peer, generation }),
        ]
    }

    /// A daemon ↔ daemon event.
    pub fn peer_event() -> impl Strategy<Value = PeerEvent> {
        prop_oneof![
            (any::<u64>(), any::<u64>(), metadata(), payload()).prop_map(
                |(route, seq, metadata, payload)| PeerEvent::Output {
                    route_id: RouteId::new(route),
                    seq,
                    metadata,
                    payload,
                }
            ),
            (any::<u64>(), close_reason()).prop_map(|(route, reason)| {
                PeerEvent::RouteTeardown {
                    route_id: RouteId::new(route),
                    reason,
                }
            }),
            (any::<u64>(), any::<u64>(), close_reason()).prop_map(|(route, final_seq, reason)| {
                PeerEvent::OutputClosed {
                    route_id: RouteId::new(route),
                    final_seq,
                    reason,
                }
            }),
            (any::<u64>(), timestamp(), any::<bool>()).prop_map(|(nonce, sent_at, is_reply)| {
                PeerEvent::Ping {
                    nonce,
                    sent_at,
                    is_reply,
                }
            }),
        ]
    }

    /// A feature set, including bits this build does not know.
    pub fn features() -> impl Strategy<Value = FeatureFlags> {
        any::<u64>().prop_map(FeatureFlags::from_bits)
    }

    /// A plausible per-connection budget.
    pub fn limits() -> impl Strategy<Value = NegotiatedLimits> {
        (
            64u64 * 1024..64 * 1024 * 1024,
            1u32..8_192,
            1u32..4_096,
            1u32..1_024,
            1u64..600_000,
            any::<bool>(),
        )
            .prop_map(
                |(payload, routes, inflight, subscriptions, heartbeat, crc)| {
                    NegotiatedLimits::new()
                        .with_max_payload_bytes(payload)
                        .with_max_routes(routes)
                        .with_max_inflight_frames(inflight)
                        .with_max_subscriptions(subscriptions)
                        .with_heartbeat_interval(DurationMs::new(heartbeat))
                        .with_require_crc(crc)
                },
            )
    }
}

/// Asserts that a message survives encoding, framing and decoding.
fn assert_round_trips<T>(message: &T, equal: impl Fn(&T, &T) -> bool)
where
    T: WireMessage + std::fmt::Debug,
{
    let payload = message.encode_to_vec().expect("a message always encodes");
    assert_eq!(
        payload.len(),
        message.encode_size_hint().expect("a hint"),
        "the size hint must be exact: {message:?}"
    );

    let decoded = T::from_payload(&payload).expect("a payload always decodes");
    assert!(equal(&decoded, message), "payload round trip: {message:?}");

    for limits in [FrameLimits::uds(), FrameLimits::network()] {
        let flags = if limits.require_crc() {
            FrameFlags::CRC
        } else {
            FrameFlags::EMPTY
        };
        let framed = message.to_frame(flags, &limits).expect("a frame");
        let decoded = T::from_bytes(&framed, &limits).expect("a frame decodes");
        assert!(equal(&decoded, message), "frame round trip: {message:?}");
    }
}

/// Asserts that appending anything to a payload makes it invalid.
fn assert_rejects_trailing<T>(message: &T, trailing: &[u8])
where
    T: WireMessage + std::fmt::Debug,
{
    if trailing.is_empty() {
        return;
    }
    let mut bytes = message.encode_to_vec().expect("a message always encodes");
    bytes.extend_from_slice(trailing);
    match T::decode_exact(&bytes) {
        Err(WireError::TrailingBytes { .. }) => {}
        // A longer suffix may make the *value* itself invalid first — an
        // over-long collection length, say. That is still a rejection, which
        // is what the property asserts.
        Err(WireError::Codec(_)) => {}
        other => panic!("trailing bytes were not rejected for {message:?}: {other:?}"),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn control_requests_round_trip(message in arb::control_request()) {
        assert_round_trips(&message, ControlRequest::bitwise_eq);
    }

    #[test]
    fn control_replies_round_trip(message in arb::control_reply()) {
        assert_round_trips(&message, ControlReply::bitwise_eq);
    }

    #[test]
    fn coordinator_events_round_trip(message in arb::coordinator_event()) {
        assert_round_trips(&message, CoordinatorEvent::bitwise_eq);
    }

    #[test]
    fn daemon_events_round_trip(message in arb::daemon_event()) {
        assert_round_trips(&message, DaemonEvent::bitwise_eq);
    }

    #[test]
    fn node_requests_round_trip(message in arb::node_request()) {
        assert_round_trips(&message, NodeRequest::bitwise_eq);
    }

    #[test]
    fn node_events_round_trip(message in arb::node_event()) {
        assert_round_trips(&message, NodeEvent::bitwise_eq);
    }

    #[test]
    fn peer_events_round_trip(message in arb::peer_event()) {
        assert_round_trips(&message, PeerEvent::bitwise_eq);
    }

    #[test]
    fn trailing_bytes_are_always_rejected(
        message in arb::peer_event(),
        trailing in proptest::collection::vec(any::<u8>(), 1..8),
    ) {
        assert_rejects_trailing(&message, &trailing);
    }

    #[test]
    fn trailing_bytes_are_rejected_for_every_family(
        request in arb::control_request(),
        event in arb::node_event(),
        trailing in proptest::collection::vec(any::<u8>(), 1..4),
    ) {
        assert_rejects_trailing(&request, &trailing);
        assert_rejects_trailing(&event, &trailing);
    }

    #[test]
    fn arbitrary_bytes_never_panic_a_frame_decoder(bytes in proptest::collection::vec(any::<u8>(), 0..1024)) {
        for limits in [FrameLimits::uds(), FrameLimits::network()] {
            // Whatever these bytes are, the result is a value or a typed error.
            if let Ok(view) = decode_frame(&bytes, &limits) {
                let _ = AnyMessage::from_frame(&view);
            }
        }
    }

    #[test]
    fn arbitrary_bytes_never_panic_a_payload_decoder(bytes in proptest::collection::vec(any::<u8>(), 0..1024)) {
        let _ = ControlRequest::from_payload(&bytes);
        let _ = ControlReply::from_payload(&bytes);
        let _ = CoordinatorEvent::from_payload(&bytes);
        let _ = DaemonEvent::from_payload(&bytes);
        let _ = NodeRequest::from_payload(&bytes);
        let _ = NodeEvent::from_payload(&bytes);
        let _ = PeerEvent::from_payload(&bytes);
    }

    #[test]
    fn a_corrupted_frame_is_an_error_never_a_panic(
        message in arb::peer_event(),
        index in 0usize..4096,
        mask in 1u8..=255,
    ) {
        let limits = FrameLimits::network();
        let mut bytes = message.to_frame(FrameFlags::CRC, &limits).expect("a frame");
        let index = index % bytes.len();
        bytes[index] ^= mask;

        match decode_frame(&bytes, &limits) {
            Ok(view) => {
                // The checksum covers everything from the flags byte on, so a
                // frame that still verifies was corrupted in the magic or the
                // version — both of which the header parser rejects — or the
                // payload decodes to something else entirely. Either way: no
                // panic, and no claim that it equals the original.
                let _ = PeerEvent::from_frame(&view);
            }
            Err(err) => {
                prop_assert!(err.is_protocol_violation(), "unexpected error {err:?}");
            }
        }
    }

    #[test]
    fn a_corrupted_unchecked_frame_never_panics(
        message in arb::node_event(),
        index in 0usize..4096,
        mask in 1u8..=255,
    ) {
        // Without a checksum, corruption reaches the payload decoder — which is
        // exactly the path this property is here to exercise.
        let limits = FrameLimits::uds();
        let mut bytes = message.to_frame(FrameFlags::EMPTY, &limits).expect("a frame");
        let index = index % bytes.len();
        bytes[index] ^= mask;
        if let Ok(view) = decode_frame(&bytes, &limits) {
            let _ = NodeEvent::from_frame(&view);
        }
    }

    #[test]
    fn a_truncated_frame_is_incomplete_never_a_panic(
        message in arb::peer_event(),
        cut in 0usize..4096,
    ) {
        let limits = FrameLimits::network();
        let bytes = message.to_frame(FrameFlags::CRC, &limits).expect("a frame");
        let cut = cut % bytes.len();
        let err = astrs_wire::decode_frame_prefix(&bytes[..cut], &limits).unwrap_err();
        prop_assert!(err.is_incomplete(), "expected an incomplete frame, got {err:?}");
    }

    #[test]
    fn a_stream_split_anywhere_reassembles(
        messages in proptest::collection::vec(arb::peer_event(), 1..6),
        chunk in 1usize..64,
    ) {
        let limits = FrameLimits::uds();
        let mut wire = Vec::new();
        {
            let mut writer = FrameWriter::new(&mut wire, limits);
            for message in &messages {
                writer.queue(message).expect("a frame");
            }
            writer.flush().expect("a flush");
        }

        let mut buffer = FrameBuffer::new(limits);
        let mut decoded = Vec::new();
        for piece in wire.chunks(chunk) {
            buffer.push(piece);
            while let Some(view) = buffer.next_frame().expect("a frame or nothing") {
                decoded.push(PeerEvent::from_frame(&view).expect("a message"));
            }
        }
        prop_assert_eq!(decoded.len(), messages.len());
        for (left, right) in decoded.iter().zip(&messages) {
            prop_assert!(left.bitwise_eq(right));
        }
    }

    #[test]
    fn a_reader_sees_exactly_what_a_writer_wrote(
        messages in proptest::collection::vec(arb::node_event(), 1..8),
    ) {
        let limits = FrameLimits::network();
        let mut wire = Vec::new();
        {
            let mut writer = FrameWriter::new(&mut wire, limits);
            for message in &messages {
                writer.queue(message).expect("a frame");
            }
            writer.flush().expect("a flush");
        }

        let mut reader = FrameReader::new(wire.as_slice(), limits);
        for expected in &messages {
            let event: NodeEvent = reader.read_message().expect("no error").expect("a frame");
            prop_assert!(event.bitwise_eq(expected));
        }
        prop_assert!(reader.read_message::<NodeEvent>().expect("no error").is_none());
    }

    #[test]
    fn feature_negotiation_is_an_intersection(left in arb::features(), right in arb::features()) {
        let agreed = left.negotiate(right);
        prop_assert_eq!(agreed, right.negotiate(left), "commutative");
        prop_assert!(left.contains(agreed), "never grants what the left end lacks");
        prop_assert!(right.contains(agreed), "never grants what the right end lacks");
        prop_assert_eq!(agreed.negotiate(agreed), agreed, "idempotent");
    }

    #[test]
    fn limit_negotiation_takes_the_stricter_side(
        left in arb::limits(),
        right in arb::limits(),
    ) {
        let agreed = left.negotiate(&right);
        prop_assert_eq!(agreed, right.negotiate(&left), "commutative");
        prop_assert!(agreed.max_payload_bytes <= left.max_payload_bytes);
        prop_assert!(agreed.max_payload_bytes <= right.max_payload_bytes);
        prop_assert!(agreed.max_routes <= left.max_routes.min(right.max_routes));
        prop_assert_eq!(
            agreed.require_crc,
            left.require_crc || right.require_crc,
            "integrity is the one field where the stricter policy is the OR"
        );
        prop_assert!(
            agreed.keepalive_timeout.as_millis() >= agreed.heartbeat_interval.as_millis(),
            "a repaired budget never times out inside one heartbeat"
        );
    }

    #[test]
    fn a_handshake_agrees_on_terms_neither_end_exceeds(
        client_features in arb::features(),
        server_features in arb::features(),
        client_limits in arb::limits(),
        server_limits in arb::limits(),
        session in any::<u128>(),
    ) {
        let token = AuthToken::from_bytes([0x5A; 32]);
        let hello = Hello::new(astrs_wire::Role::Node, token.clone())
            .with_features(client_features)
            .with_limits(client_limits);
        let acceptor = Acceptor::new(token)
            .with_features(server_features)
            .with_limits(server_limits);

        let outcome = negotiate(
            &hello,
            &acceptor,
            SessionAssignment::Fresh(SessionId::from_u128(session)),
        );
        let welcome = outcome.welcome().expect("a valid greeting is accepted").clone();

        prop_assert!(client_features.contains(welcome.features));
        prop_assert!(server_features.contains(welcome.features));
        prop_assert!(!welcome.features.has_unknown());
        prop_assert!(welcome.limits.max_payload_bytes <= client_limits.max_payload_bytes);
        prop_assert!(welcome.limits.max_payload_bytes <= server_limits.max_payload_bytes);
        prop_assert!(welcome.protocol <= hello.protocol);

        // And the initiator's own check agrees with the acceptor's.
        let session = accept_welcome(&hello, &welcome).expect("the welcome is well formed");
        prop_assert_eq!(session.features, welcome.features);
        prop_assert_eq!(session.limits, welcome.limits);
    }

    #[test]
    fn a_wrong_token_is_always_refused(
        token in proptest::array::uniform32(any::<u8>()),
        presented in proptest::array::uniform32(any::<u8>()),
    ) {
        prop_assume!(token != presented);
        let hello = Hello::new(astrs_wire::Role::Node, AuthToken::from_bytes(presented));
        let acceptor = Acceptor::new(AuthToken::from_bytes(token));
        let outcome = negotiate(&hello, &acceptor, SessionAssignment::Fresh(SessionId::NIL));
        prop_assert!(outcome.refused().is_some());
    }

    #[test]
    fn the_frame_kind_survives_every_message(message in arb::peer_event()) {
        let limits = FrameLimits::uds();
        let bytes = message.to_frame(FrameFlags::EMPTY, &limits).expect("a frame");
        let view = decode_frame(&bytes, &limits).expect("a frame");
        prop_assert_eq!(view.kind(), FrameKind::PeerEvent);
        let message = AnyMessage::from_frame(&view).expect("a message");
        prop_assert!(
            matches!(message, AnyMessage::PeerEvent(_)),
            "the dispatcher must pick the peer family"
        );
    }

    #[test]
    fn an_oversize_payload_is_refused_rather_than_encoded(
        payload in proptest::collection::vec(any::<u8>(), 64..512),
        cap in 1usize..32,
    ) {
        let limits = FrameLimits::uds().with_max_payload_bytes(cap);
        let message = PeerEvent::Output {
            route_id: RouteId::FIRST,
            seq: 0,
            metadata: Metadata::default(),
            payload,
        };
        let framed = message.to_frame(FrameFlags::EMPTY, &limits);
        prop_assert!(
            matches!(framed, Err(WireError::FrameTooLarge { .. })),
            "an oversize payload must be refused"
        );
    }

    #[test]
    fn log_levels_filter_monotonically(
        level in 0u8..5,
        filter in 0u8..5,
    ) {
        let levels = LogLevel::ALL;
        let record = levels[usize::from(level)];
        let filter = levels[usize::from(filter)];
        prop_assert_eq!(
            record.is_enabled_at(filter),
            record.as_u8() <= filter.as_u8()
        );
    }

    #[test]
    fn compression_flags_round_trip_through_the_header(
        compression in prop_oneof![
            Just(Compression::None),
            Just(Compression::Lz4),
            Just(Compression::Zstd),
        ],
        crc in any::<bool>(),
    ) {
        let flags = FrameFlags::EMPTY.with_crc(crc).with_compression(compression);
        let limits = FrameLimits::uds();
        let bytes = astrs_wire::encode_frame(FrameKind::Data, flags, b"opaque", &limits)
            .expect("a frame");
        let view = decode_frame(&bytes, &limits).expect("a frame");
        prop_assert_eq!(view.flags().compression(), compression);
        prop_assert_eq!(view.flags().has_crc(), crc);
        prop_assert_eq!(view.payload(), b"opaque");
    }
}

#[test]
fn every_committed_sample_round_trips_through_both_flavours() {
    // The generated cases above explore shapes; this asserts the *documented*
    // samples — the ones the snapshot freezes — survive the real readers.
    let limits = FrameLimits::network();
    let mut wire = Vec::new();
    let mut writer = FrameWriter::new(&mut wire, limits);
    for request in astrs_wire::samples::control_requests().expect("samples") {
        writer.queue(&request).expect("a frame");
    }
    writer.flush().expect("a flush");

    let mut reader = FrameReader::new(wire.as_slice(), limits);
    for expected in astrs_wire::samples::control_requests().expect("samples") {
        let request: ControlRequest = reader.read_message().expect("no error").expect("a frame");
        assert!(request.bitwise_eq(&expected));
    }
    assert!(
        reader
            .read_message::<ControlRequest>()
            .expect("no error")
            .is_none()
    );
}
