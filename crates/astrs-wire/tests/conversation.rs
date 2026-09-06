//! End-to-end conversations over real duplex pipes.
//!
//! The unit tests check each message in isolation; these check that the pieces
//! compose into the exchanges blueprint §7 actually describes — a node
//! attaching to its daemon, a CLI asking a coordinator for something, two
//! daemons opening a route — with both halves running concurrently over a
//! `tokio::io::duplex` pipe, framed by the crate's own readers and writers.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_time::HlcTimestamp;
use astrs_wire::{
    Acceptor, AsyncFrameReader, AsyncFrameWriter, AuthToken, Compression, ControlReply,
    ControlRequest, DataId, DataflowId, FeatureFlags, FrameLimits, HandshakeOutcome, Hello,
    Metadata, NegotiatedLimits, NodeEvent, NodeHandshake, NodeId, NodeRequest, OutputPayload,
    PeerEvent, Plane, Role, RoleSet, RouteAcceptance, RouteId, RouteSpec, SessionAssignment,
    SessionId, StopCause, WireError, accept_welcome, negotiate, samples,
};
use tokio::io::duplex;

/// The cluster token both ends of every test share.
fn token() -> AuthToken {
    AuthToken::from_bytes([0x2B; 32])
}

/// Runs a future on a fresh current-thread runtime.
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime")
        .block_on(future)
}

#[test]
fn a_node_attaches_to_its_daemon_and_exchanges_a_message() {
    block_on(async {
        let limits = FrameLimits::uds();
        let (node_side, daemon_side) = duplex(64 * 1024);
        let (node_read, node_write) = tokio::io::split(node_side);
        let (daemon_read, daemon_write) = tokio::io::split(daemon_side);

        // ── the daemon ────────────────────────────────────────────────────
        let daemon = tokio::spawn(async move {
            let mut reader = AsyncFrameReader::new(daemon_read, limits);
            let mut writer = AsyncFrameWriter::new(daemon_write, limits);

            // 1. The handshake, on the control family, whatever the leg (§7.2).
            let greeting = match reader
                .read_message::<ControlRequest>()
                .await
                .expect("a greeting")
                .expect("a frame")
            {
                ControlRequest::Hello(hello) => hello,
                other => panic!("the first frame must be a greeting, got {other:?}"),
            };
            assert_eq!(greeting.role, Role::Node);

            let acceptor = Acceptor::new(token())
                .with_accepted_roles(RoleSet::NODES)
                .with_features(FeatureFlags::daemon_defaults())
                .with_limits(NegotiatedLimits::uds());
            let session = SessionId::from_u128(0x5E55);
            let reply = match negotiate(&greeting, &acceptor, SessionAssignment::Fresh(session)) {
                HandshakeOutcome::Accepted { welcome, .. } => ControlReply::Welcome(welcome),
                HandshakeOutcome::Refused(refused) => ControlReply::Refused(refused),
            };
            writer.send(&reply).await.expect("the welcome is sent");

            // 2. Registration.
            let handshake = match reader
                .read_message::<NodeRequest>()
                .await
                .expect("a request")
                .expect("a frame")
            {
                NodeRequest::Register(handshake) => handshake,
                other => panic!("expected a registration, got {other:?}"),
            };
            assert_eq!(handshake.node.as_str(), "camera");
            writer
                .send(&NodeEvent::Registered {
                    spec: Box::new(samples::sample_spawn_spec().expect("a spec")),
                    session,
                })
                .await
                .expect("the acknowledgement is sent");

            // 3. The node subscribes; the daemon delivers one input.
            let subscribe = reader
                .read_message::<NodeRequest>()
                .await
                .expect("a request")
                .expect("a frame");
            assert!(matches!(subscribe, NodeRequest::Subscribe { .. }));
            writer
                .send(&NodeEvent::Input {
                    id: DataId::new("tick").expect("valid"),
                    source: "clock/tick".parse().expect("valid"),
                    metadata: Metadata::new(HlcTimestamp::new(1_000, 0)),
                    payload: vec![1, 2, 3, 4],
                })
                .await
                .expect("the input is delivered");

            // 4. The node publishes; the daemon upgrades the route (§6.3).
            let sent = reader
                .read_message::<NodeRequest>()
                .await
                .expect("a request")
                .expect("a frame");
            assert!(sent.is_send());
            assert_eq!(sent.payload_len(), 4);

            writer
                .send(&NodeEvent::RouteUpgrade {
                    output: DataId::new("image").expect("valid"),
                    segment: astrs_wire::ShmSegmentSpec::new("astrs/df/camera/1/image", 1, 8, 4096),
                    consumers: vec!["detector/frames".parse().expect("valid")],
                })
                .await
                .expect("the upgrade is sent");

            let ack = reader
                .read_message::<NodeRequest>()
                .await
                .expect("a request")
                .expect("a frame");
            assert!(matches!(
                ack,
                NodeRequest::RouteUpgradeAck { accepted: true, .. }
            ));

            // 5. Shutdown.
            writer
                .send(&NodeEvent::Stop {
                    cause: StopCause::Requested,
                    grace: None,
                })
                .await
                .expect("the stop is sent");
            writer.shutdown().await.expect("a clean close");
        });

        // ── the node ──────────────────────────────────────────────────────
        let mut reader = AsyncFrameReader::new(node_read, limits);
        let mut writer = AsyncFrameWriter::new(node_write, limits);

        let hello = Hello::new(Role::Node, token())
            .with_features(FeatureFlags::SHM_ZERO_COPY)
            .with_label("camera");
        writer
            .send(&ControlRequest::Hello(hello.clone()))
            .await
            .expect("the greeting is sent");

        let welcome = match reader
            .read_message::<ControlReply>()
            .await
            .expect("a reply")
            .expect("a frame")
        {
            ControlReply::Welcome(welcome) => welcome,
            other => panic!("expected a welcome, got {other}"),
        };
        let session = accept_welcome(&hello, &welcome).expect("the welcome is well formed");
        assert!(session.supports(FeatureFlags::SHM_ZERO_COPY));
        assert_eq!(session.session_id, SessionId::from_u128(0x5E55));

        writer
            .send(&NodeRequest::Register(NodeHandshake::new(
                DataflowId::from_u128(1),
                NodeId::new("camera").expect("valid"),
                1,
            )))
            .await
            .expect("the registration is sent");

        let registered = reader
            .read_message::<NodeEvent>()
            .await
            .expect("an event")
            .expect("a frame");
        assert!(matches!(registered, NodeEvent::Registered { .. }));

        writer
            .send(&NodeRequest::Subscribe { inputs: Vec::new() })
            .await
            .expect("the subscription is sent");

        let input = reader
            .read_message::<NodeEvent>()
            .await
            .expect("an event")
            .expect("a frame");
        assert!(input.is_input());
        assert_eq!(input.payload_len(), 4);

        writer
            .send(&NodeRequest::SendMessage {
                output: DataId::new("image").expect("valid"),
                metadata: Metadata::new(HlcTimestamp::new(1_001, 0)),
                payload: OutputPayload::inline(vec![9, 9, 9, 9]),
            })
            .await
            .expect("the output is sent");

        let upgrade = reader
            .read_message::<NodeEvent>()
            .await
            .expect("an event")
            .expect("a frame");
        assert!(upgrade.is_route_change());

        writer
            .send(&NodeRequest::RouteUpgradeAck {
                output: DataId::new("image").expect("valid"),
                accepted: true,
                reason: None,
            })
            .await
            .expect("the acknowledgement is sent");

        let stop = reader
            .read_message::<NodeEvent>()
            .await
            .expect("an event")
            .expect("a frame");
        assert!(stop.is_terminal());

        // The daemon closed cleanly, so the stream ends between frames.
        assert!(
            reader
                .read_message::<NodeEvent>()
                .await
                .expect("a clean end")
                .is_none()
        );

        daemon.await.expect("the daemon task finished");
    });
}

#[test]
fn a_cli_request_is_answered_in_order() {
    block_on(async {
        let limits = FrameLimits::network();
        let (cli_side, coordinator_side) = duplex(64 * 1024);
        let (cli_read, cli_write) = tokio::io::split(cli_side);
        let (coordinator_read, coordinator_write) = tokio::io::split(coordinator_side);

        let coordinator = tokio::spawn(async move {
            let mut reader = AsyncFrameReader::new(coordinator_read, limits);
            let mut writer = AsyncFrameWriter::new(coordinator_write, limits);
            let mut answered = 0usize;
            while let Some(request) = reader
                .read_message::<ControlRequest>()
                .await
                .expect("a request")
            {
                // §16: a read verb and a mutating verb are distinguishable
                // without decoding the payload's meaning.
                let reply = if request.is_mutating() {
                    ControlReply::Ok
                } else {
                    ControlReply::DataflowList {
                        dataflows: Vec::new(),
                        nodes: Vec::new(),
                    }
                };
                writer.send(&reply).await.expect("a reply");
                answered += 1;
            }
            writer.shutdown().await.expect("a clean close");
            answered
        });

        let mut reader = AsyncFrameReader::new(cli_read, limits);
        let mut writer = AsyncFrameWriter::new(cli_write, limits);

        let requests = [
            ControlRequest::List { all: true },
            ControlRequest::Stop {
                dataflow: DataflowId::from_u128(1),
                grace: None,
            },
            ControlRequest::ConnectedDaemons {
                include_unreachable: false,
            },
        ];
        for request in &requests {
            writer.send(request).await.expect("a request");
            let reply = reader
                .read_message::<ControlReply>()
                .await
                .expect("a reply")
                .expect("a frame");
            assert!(reply.is_success());
            if request.is_mutating() {
                assert_eq!(reply, ControlReply::Ok);
            } else {
                assert!(matches!(reply, ControlReply::DataflowList { .. }));
            }
        }
        writer.shutdown().await.expect("a clean close");

        assert_eq!(coordinator.await.expect("the coordinator finished"), 3);
    });
}

#[test]
fn two_daemons_open_a_route_and_carry_a_payload() {
    block_on(async {
        let limits = FrameLimits::network();
        let (producer_side, consumer_side) = duplex(64 * 1024);
        let (producer_read, producer_write) = tokio::io::split(producer_side);
        let (consumer_read, consumer_write) = tokio::io::split(consumer_side);

        let consumer = tokio::spawn(async move {
            let mut reader = AsyncFrameReader::new(consumer_read, limits);
            let mut writer = AsyncFrameWriter::new(consumer_write, limits);

            let setup = reader
                .read_message::<PeerEvent>()
                .await
                .expect("an event")
                .expect("a frame");
            let route_id = setup.route_id().expect("a route handle");
            writer
                .send(&PeerEvent::RouteAccept {
                    route_id,
                    acceptance: RouteAcceptance::accepted(Plane::Quic, Compression::None, 1 << 20),
                })
                .await
                .expect("the acceptance is sent");

            let mut payload_bytes = 0usize;
            let mut closed = false;
            while let Some(event) = reader.read_message::<PeerEvent>().await.expect("an event") {
                payload_bytes += event.payload_len();
                if matches!(event, PeerEvent::RouteTeardown { .. }) {
                    closed = true;
                    break;
                }
            }
            writer.shutdown().await.expect("a clean close");
            (payload_bytes, closed)
        });

        let mut reader = AsyncFrameReader::new(producer_read, limits);
        let mut writer = AsyncFrameWriter::new(producer_write, limits);

        let route =
            RouteSpec::new(samples::sample_route_key().expect("a key")).with_plane(Plane::Quic);
        writer
            .send(&PeerEvent::RouteSetup {
                route_id: RouteId::FIRST,
                route,
                generation: 1,
                type_urn: None,
                max_payload_bytes: 1 << 20,
                pool_hint_bytes: None,
            })
            .await
            .expect("the setup is sent");

        let accept = reader
            .read_message::<PeerEvent>()
            .await
            .expect("an event")
            .expect("a frame");
        match accept {
            PeerEvent::RouteAccept { acceptance, .. } => {
                assert!(acceptance.is_accepted());
                assert_eq!(acceptance.plane(), Some(Plane::Quic));
            }
            other => panic!("expected an acceptance, got {other}"),
        }

        for seq in 0..4u64 {
            writer
                .queue(&PeerEvent::Output {
                    route_id: RouteId::FIRST,
                    seq,
                    metadata: Metadata::new(HlcTimestamp::new(seq, 0)),
                    payload: vec![0xAB; 64],
                })
                .expect("an output");
        }
        writer
            .queue(&PeerEvent::OutputClosed {
                route_id: RouteId::FIRST,
                final_seq: 3,
                reason: astrs_wire::RouteCloseReason::ProducerFinished,
            })
            .expect("a close");
        writer
            .queue(&PeerEvent::RouteTeardown {
                route_id: RouteId::FIRST,
                reason: astrs_wire::RouteCloseReason::DataflowStopped,
            })
            .expect("a teardown");
        writer.flush().await.expect("a flush");
        writer.shutdown().await.expect("a clean close");

        let (payload_bytes, closed) = consumer.await.expect("the consumer finished");
        assert_eq!(payload_bytes, 4 * 64);
        assert!(closed);
    });
}

#[test]
fn a_refused_handshake_ends_the_conversation() {
    block_on(async {
        let limits = FrameLimits::uds();
        let (client_side, server_side) = duplex(8 * 1024);
        let (client_read, client_write) = tokio::io::split(client_side);
        let (server_read, server_write) = tokio::io::split(server_side);

        let server = tokio::spawn(async move {
            let mut reader = AsyncFrameReader::new(server_read, limits);
            let mut writer = AsyncFrameWriter::new(server_write, limits);
            let greeting = match reader
                .read_message::<ControlRequest>()
                .await
                .expect("a greeting")
                .expect("a frame")
            {
                ControlRequest::Hello(hello) => hello,
                other => panic!("expected a greeting, got {other:?}"),
            };
            // The token does not match, so the connection is refused (§16).
            let acceptor = Acceptor::new(AuthToken::from_bytes([0xFF; 32]));
            let reply = match negotiate(
                &greeting,
                &acceptor,
                SessionAssignment::Fresh(SessionId::NIL),
            ) {
                HandshakeOutcome::Accepted { welcome, .. } => ControlReply::Welcome(welcome),
                HandshakeOutcome::Refused(refused) => ControlReply::Refused(refused),
            };
            writer.send(&reply).await.expect("the refusal is sent");
            writer.shutdown().await.expect("a clean close");
        });

        let mut reader = AsyncFrameReader::new(client_read, limits);
        let mut writer = AsyncFrameWriter::new(client_write, limits);
        writer
            .send(&ControlRequest::Hello(Hello::new(Role::Cli, token())))
            .await
            .expect("the greeting is sent");

        let reply = reader
            .read_message::<ControlReply>()
            .await
            .expect("a reply")
            .expect("a frame");
        assert!(reply.is_error());
        assert_eq!(
            reply.error_code(),
            Some(astrs_wire::ErrorCode::PermissionDenied)
        );
        assert!(reply.refused().is_some());
        assert!(
            reader
                .read_message::<ControlReply>()
                .await
                .expect("a clean end")
                .is_none()
        );

        server.await.expect("the server task finished");
    });
}

#[test]
fn a_peer_that_hangs_up_mid_frame_is_reported_as_truncated() {
    block_on(async {
        let limits = FrameLimits::uds();
        let (mut sender, receiver) = duplex(8 * 1024);

        let half = tokio::spawn(async move {
            let mut reader = AsyncFrameReader::new(receiver, limits);
            reader.read_frame().await
        });

        // Write a header claiming a payload, then hang up.
        let frame = PeerEvent::ping(1, HlcTimestamp::new(1, 0))
            .to_frame_bytes(limits)
            .expect("a frame");
        tokio::io::AsyncWriteExt::write_all(&mut sender, &frame[..frame.len() - 2])
            .await
            .expect("a partial write");
        drop(sender);

        assert!(matches!(
            half.await.expect("the reader task finished"),
            Err(WireError::Truncated { .. })
        ));
    });
}

/// A small helper so the truncation test reads as intent rather than mechanics.
trait ToFrameBytes {
    /// Encodes this message as a complete frame.
    fn to_frame_bytes(&self, limits: FrameLimits) -> Result<Vec<u8>, WireError>;
}

impl<T: astrs_wire::WireMessage> ToFrameBytes for T {
    fn to_frame_bytes(&self, limits: FrameLimits) -> Result<Vec<u8>, WireError> {
        self.to_frame(astrs_wire::FrameFlags::EMPTY, &limits)
    }
}
