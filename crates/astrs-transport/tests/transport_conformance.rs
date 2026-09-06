//! End-to-end conformance for the AstRS transport, over real sockets.
//!
//! The unit tests inside the crate check each layer against in-memory duplex
//! pipes; this suite checks the whole stack against the kernel. That difference
//! matters more than it sounds: a duplex pipe delivers writes atomically and in
//! one piece, while a socket splits, coalesces and reorders the *bytes* around
//! frame boundaries. Every property below has been broken at some point by a
//! layer that was correct against a pipe.
//!
//! The suite covers the blueprint's transport obligations:
//!
//! | Test | Blueprint |
//! |---|---|
//! | loopback echo, UDS and TCP | §6.4 |
//! | handshake refusal on a bad token / role / protocol | §7.2 |
//! | max-frame enforcement on send *and* receive | §7.1, §7.2 |
//! | corruption becomes a checksum error, never a panic | §7.1 |
//! | reconnect storm: restarts, resumes, epoch increments | §12 |
//! | route-mux fairness under saturation | §6.4 |
//! | compression round trip, both codecs, at the threshold | §6.4, §7.1 |
//! | one hundred concurrent routes | §6.4 |

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use astrs_transport::backend::tcp::{self, TcpListener};
use astrs_transport::backend::uds::{self, UdsListener};
use astrs_transport::{
    CompressionPolicy, Connection, ConnectionCounters, ConnectionEvent, FramedStream,
    HandshakeParams, LocalIdentity, MuxChannels, MuxConfig, OverflowPolicy, ReconnectingConnection,
    StreamConnection, TransportAddr, TransportConfig, TransportError, acceptor_from_config,
};
use astrs_wire::{
    AuthToken, Compression, ControlReply, ControlRequest, FrameFlags, FrameKind, FrameLimits,
    Hello, RefusalReason, Role, RoleSet, SessionAssignment, SessionId,
};
use tokio::io::AsyncWriteExt;

/// A generous ceiling for "this must not hang", not a latency assertion.
const GENEROUS: Duration = Duration::from_secs(30);

/// The cluster token every test in this file authenticates with.
fn token() -> AuthToken {
    AuthToken::from_bytes([0x5e; 32])
}

/// A unique, short socket path under the platform temp directory.
///
/// Unix socket paths are length-limited (104 bytes on macOS), so the name is
/// kept short deliberately.
fn socket_path(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "astrs-it-{tag}-{}-{unique}.sock",
        std::process::id()
    ))
}

/// The loopback address with an ephemeral port.
fn loopback() -> SocketAddr {
    "127.0.0.1:0".parse().expect("a valid loopback address")
}

/// Client-side handshake parameters for `role`.
fn params(config: &TransportConfig, role: Role, crc: bool) -> HandshakeParams {
    HandshakeParams::from_config(config, LocalIdentity::new(role), token(), crc)
}

/// One connected pair, whichever backend produced it.
struct Pair {
    client: StreamConnection,
    client_channels: MuxChannels,
    server: StreamConnection,
    server_channels: MuxChannels,
    /// Kept alive so the socket file is not unlinked mid-test.
    _listener: Option<UdsListener>,
}

/// Establishes a pair over a real Unix socket.
async fn uds_pair(tag: &str, config: TransportConfig) -> Pair {
    let path = socket_path(tag);
    let listener = UdsListener::bind(&path, config.clone()).expect("bind");
    let acceptor = acceptor_from_config(&config, token(), RoleSet::ALL, false);

    let accept_config = config.clone();
    let server = tokio::spawn(async move {
        let outcome = listener
            .accept(&acceptor, SessionAssignment::Fresh(SessionId::from_u128(1)))
            .await;
        (outcome, listener)
    });
    let _ = accept_config;

    let (client, client_channels) =
        uds::connect(&path, &config, &params(&config, Role::Node, false))
            .await
            .expect("connect");
    let ((server, server_channels), listener) = {
        let (outcome, listener) = server.await.expect("server task");
        (outcome.expect("accept"), listener)
    };

    Pair {
        client,
        client_channels,
        server,
        server_channels,
        _listener: Some(listener),
    }
}

/// Establishes a pair over a real TCP socket.
async fn tcp_pair(config: TransportConfig) -> Pair {
    let listener = TcpListener::bind(loopback(), config.clone())
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let acceptor = acceptor_from_config(&config, token(), RoleSet::ALL, true);

    let server = tokio::spawn(async move {
        listener
            .accept(&acceptor, SessionAssignment::Fresh(SessionId::from_u128(1)))
            .await
    });
    let (client, client_channels) = tcp::connect(addr, &config, &params(&config, Role::Peer, true))
        .await
        .expect("connect");
    let (server, server_channels) = server.await.expect("server task").expect("accept");

    Pair {
        client,
        client_channels,
        server,
        server_channels,
        _listener: None,
    }
}

// ---------------------------------------------------------------------------
// Loopback echo, per backend
// ---------------------------------------------------------------------------

/// Bounces `count` control frames off the peer, checking every one.
async fn echo_control(pair: &mut Pair, count: usize) {
    for index in 0..count {
        let payload = format!("frame-{index}");
        pair.client
            .open_control()
            .send(FrameKind::PeerEvent, payload.as_bytes())
            .await
            .expect("send");

        let frame = tokio::time::timeout(GENEROUS, pair.server_channels.control.recv())
            .await
            .expect("no stall")
            .expect("a control frame");
        assert_eq!(frame.payload(), payload.as_bytes());

        pair.server
            .open_control()
            .send(FrameKind::PeerEvent, frame.payload())
            .await
            .expect("echo");
        let echoed = tokio::time::timeout(GENEROUS, pair.client_channels.control.recv())
            .await
            .expect("no stall")
            .expect("an echoed frame");
        assert_eq!(echoed.payload(), payload.as_bytes());
    }
}

#[tokio::test]
async fn uds_loopback_echoes_every_frame() {
    let mut pair = uds_pair("echo", TransportConfig::uds()).await;
    echo_control(&mut pair, 32).await;

    assert_eq!(pair.client.peer().plane, astrs_wire::Plane::Uds);
    assert!(
        pair.server.peer().credentials.is_some(),
        "a Unix socket must carry kernel-vouched credentials"
    );
    assert!(
        !pair.client.session().limits.require_crc,
        "a Unix socket leg does not need a checksum (§7.1)"
    );
}

#[tokio::test]
async fn tcp_loopback_echoes_every_frame() {
    let mut pair = tcp_pair(TransportConfig::new()).await;
    echo_control(&mut pair, 32).await;

    assert_eq!(pair.client.peer().plane, astrs_wire::Plane::Tcp);
    assert!(
        pair.client.session().limits.require_crc,
        "a network leg always checksums (§7.1)"
    );
}

#[tokio::test]
async fn both_backends_carry_route_payloads_end_to_end() {
    for (name, mut pair) in [
        ("uds", uds_pair("routes", TransportConfig::uds()).await),
        ("tcp", tcp_pair(TransportConfig::new()).await),
    ] {
        let stream = pair.client.open_route(b"camera->detector").expect("open");
        let mut accepted = tokio::time::timeout(GENEROUS, pair.server_channels.accepts.accept())
            .await
            .expect("no stall")
            .expect("an inbound route");
        assert_eq!(accepted.descriptor(), b"camera->detector", "{name}");

        for index in 0..16u16 {
            stream
                .sender()
                .send(FrameKind::Data, &index.to_le_bytes())
                .await
                .expect("send");
        }
        for index in 0..16u16 {
            let frame = tokio::time::timeout(GENEROUS, accepted.receiver_mut().recv())
                .await
                .expect("no stall")
                .expect("a route frame");
            assert_eq!(frame.payload(), &index.to_le_bytes(), "{name}");
        }
    }
}

// ---------------------------------------------------------------------------
// Handshake refusal (§7.2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_protocol_from_before_the_freeze_is_refused_with_the_supported_range() {
    let config = TransportConfig::new();
    let listener = TcpListener::bind(loopback(), config.clone())
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let acceptor = acceptor_from_config(&config, token(), RoleSet::ALL, true);

    let server = tokio::spawn(async move {
        listener
            .accept(&acceptor, SessionAssignment::Fresh(SessionId::from_u128(1)))
            .await
    });

    // Speak the frame format by hand, with a protocol number from before the
    // snapshot froze the enums.
    let socket = tokio::net::TcpStream::connect(addr).await.expect("dial");
    let mut framed =
        FramedStream::new(socket, FrameLimits::network(), ConnectionCounters::shared());
    let mut ancient = Hello::new(Role::Peer, token());
    ancient.protocol = 0;
    framed
        .send_message(&ControlRequest::Hello(ancient))
        .await
        .expect("greet");

    let reply: ControlReply = framed
        .expect_message(FrameKind::ControlReply)
        .await
        .expect("an answer");
    match reply {
        ControlReply::Refused(refused) => {
            assert!(
                matches!(
                    refused.reason,
                    RefusalReason::ProtocolTooOld { peer: 0, .. }
                ),
                "expected a protocol refusal, got {:?}",
                refused.reason
            );
            assert_eq!(refused.max_protocol, astrs_wire::PROTOCOL_VERSION);
        }
        other => panic!("expected a refusal, got {other:?}"),
    }

    let err = server.await.expect("server task").unwrap_err();
    assert!(matches!(err, TransportError::Refused(_)));
}

#[tokio::test]
async fn a_bad_cluster_token_is_refused_and_the_listener_survives() {
    let config = TransportConfig::new();
    let listener = TcpListener::bind(loopback(), config.clone())
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let acceptor = acceptor_from_config(&config, token(), RoleSet::ALL, true);

    let server = tokio::spawn(async move {
        let first = listener
            .accept(&acceptor, SessionAssignment::Fresh(SessionId::from_u128(1)))
            .await;
        let second = listener
            .accept(&acceptor, SessionAssignment::Fresh(SessionId::from_u128(2)))
            .await;
        (first, second)
    });

    let bad = HandshakeParams::from_config(
        &config,
        LocalIdentity::new(Role::Peer),
        AuthToken::from_bytes([0xff; 32]),
        true,
    );
    let err = tcp::connect(addr, &config, &bad).await.unwrap_err();
    assert!(!err.is_retryable(), "the token will still be wrong");
    match &err {
        TransportError::Refused(refused) => assert_eq!(refused.reason, RefusalReason::BadAuth),
        other => panic!("expected a refusal, got {other:?}"),
    }

    // The listener must still serve the next, honest peer.
    let (good, _channels) = tcp::connect(addr, &config, &params(&config, Role::Peer, true))
        .await
        .expect("the listener must survive a refusal");
    let (first, second) = server.await.expect("server task");
    assert!(first.is_err());
    assert!(second.is_ok());
    assert!(!good.is_closed());
}

#[tokio::test]
async fn a_role_the_endpoint_does_not_serve_is_refused() {
    let config = TransportConfig::uds();
    let path = socket_path("role");
    let listener = UdsListener::bind(&path, config.clone()).expect("bind");
    // A node socket serves nodes and nobody else.
    let acceptor = acceptor_from_config(&config, token(), RoleSet::NODES, false);

    let server = tokio::spawn(async move {
        let outcome = listener
            .accept(&acceptor, SessionAssignment::Fresh(SessionId::from_u128(1)))
            .await;
        (outcome, listener)
    });

    let err = uds::connect(&path, &config, &params(&config, Role::Cli, false))
        .await
        .unwrap_err();
    match err {
        TransportError::Refused(refused) => assert!(matches!(
            refused.reason,
            RefusalReason::RoleNotPermitted { role: Role::Cli }
        )),
        other => panic!("expected a role refusal, got {other:?}"),
    }
    let (outcome, _listener) = server.await.expect("server task");
    assert!(outcome.is_err());
}

// ---------------------------------------------------------------------------
// Max-frame enforcement, both directions (§7.1, §7.2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_negotiated_ceiling_is_enforced_on_send() {
    // `astrs-wire` refuses to negotiate below `MIN_USABLE_PAYLOAD_BYTES`
    // (64 KiB), which is the floor the protocol's own messages need.
    let ceiling = 128 * 1024;
    let config = TransportConfig::new()
        .with_limits(astrs_wire::NegotiatedLimits::network().with_max_payload_bytes(ceiling));
    let pair = tcp_pair(config).await;

    assert_eq!(pair.client.session().limits.max_payload_bytes, ceiling);
    assert_eq!(pair.server.session().limits.max_payload_bytes, ceiling);

    let stream = pair.client.open_route(b"").expect("open");
    // Comfortably inside the ceiling, allowing for the mux header.
    stream
        .sender()
        .send(FrameKind::Data, &vec![0u8; ceiling as usize - 64])
        .await
        .expect("a legal frame must go through");

    let err = stream
        .sender()
        .send(FrameKind::Data, &vec![0u8; ceiling as usize + 1])
        .await
        .unwrap_err();
    match err {
        TransportError::FrameTooLarge { limit, .. } => assert_eq!(limit as u64, ceiling),
        other => panic!("expected a size refusal, got {other:?}"),
    }
    assert!(
        !pair.client.is_closed(),
        "refusing one oversize frame must not kill the connection"
    );
}

#[tokio::test]
async fn the_negotiated_ceiling_is_enforced_on_receive() {
    // A peer that ignores the ceiling: framed by hand, with a payload the
    // acceptor's reader must refuse.
    let ceiling = 128 * 1024u64;
    let config = TransportConfig::new()
        .with_limits(astrs_wire::NegotiatedLimits::network().with_max_payload_bytes(ceiling));
    let listener = TcpListener::bind(loopback(), config.clone())
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let acceptor = acceptor_from_config(&config, token(), RoleSet::ALL, true);

    let server = tokio::spawn(async move {
        listener
            .accept(&acceptor, SessionAssignment::Fresh(SessionId::from_u128(1)))
            .await
    });

    // Complete an honest handshake first, then cheat.
    let socket = tokio::net::TcpStream::connect(addr).await.expect("dial");
    let mut framed =
        FramedStream::new(socket, FrameLimits::network(), ConnectionCounters::shared());
    let hello = Hello::new(Role::Peer, token())
        .with_limits(astrs_wire::NegotiatedLimits::network().with_max_payload_bytes(ceiling));
    framed
        .send_message(&ControlRequest::Hello(hello))
        .await
        .expect("greet");
    let reply: ControlReply = framed
        .expect_message(FrameKind::ControlReply)
        .await
        .expect("an answer");
    assert!(matches!(reply, ControlReply::Welcome(_)));

    let (server_conn, _channels) = server.await.expect("server task").expect("accept");

    // Now send a frame far above the agreed ceiling. The reader must refuse it
    // and close, rather than allocating what the peer asked for.
    framed.set_limits(FrameLimits::network());
    // The write itself may fail with a reset: the reader refuses the frame
    // from its declared length alone, long before the last byte arrives. Both
    // outcomes are the guard working.
    let _ = framed
        .send_raw(FrameKind::Data, Compression::None, &vec![0u8; 1 << 20])
        .await;

    for _ in 0..300 {
        if server_conn.is_closed() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        server_conn.is_closed(),
        "an oversize frame must end the connection, not be accepted"
    );
}

// ---------------------------------------------------------------------------
// Corruption (§7.1)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_corrupted_frame_is_a_checksum_error_and_never_a_panic() {
    let config = TransportConfig::new();
    let listener = TcpListener::bind(loopback(), config.clone())
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let acceptor = acceptor_from_config(&config, token(), RoleSet::ALL, true);

    let server = tokio::spawn(async move {
        listener
            .accept(&acceptor, SessionAssignment::Fresh(SessionId::from_u128(1)))
            .await
    });

    let mut socket = tokio::net::TcpStream::connect(addr).await.expect("dial");
    {
        let mut framed = FramedStream::new(
            &mut socket,
            FrameLimits::network(),
            ConnectionCounters::shared(),
        );
        framed
            .send_message(&ControlRequest::Hello(Hello::new(Role::Peer, token())))
            .await
            .expect("greet");
        let reply: ControlReply = framed
            .expect_message(FrameKind::ControlReply)
            .await
            .expect("an answer");
        assert!(matches!(reply, ControlReply::Welcome(_)));
    }

    let (server_conn, _channels) = server.await.expect("server task").expect("accept");

    // Build a valid checksummed frame, then flip a payload bit on the wire.
    let mut bytes = astrs_wire::encode_frame(
        FrameKind::PeerEvent,
        FrameFlags::CRC,
        b"the quick brown fox jumps over the lazy dog",
        &FrameLimits::network(),
    )
    .expect("encode");
    let midpoint = bytes.len() / 2;
    bytes[midpoint] ^= 0xff;
    socket.write_all(&bytes).await.expect("write");
    socket.flush().await.expect("flush");

    for _ in 0..300 {
        if server_conn.is_closed() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        server_conn.is_closed(),
        "a corrupt frame must close the link"
    );
    let reason = server_conn.close_reason().expect("a close reason");
    assert!(
        matches!(reason, astrs_transport::CloseReason::Protocol { .. }),
        "expected a protocol close, got {reason:?}"
    );
    assert!(server_conn.stats().connection.errors > 0);
}

#[tokio::test]
async fn a_stream_that_ends_mid_frame_is_a_truncation_not_a_short_read() {
    let config = TransportConfig::new();
    let listener = TcpListener::bind(loopback(), config.clone())
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let acceptor = acceptor_from_config(&config, token(), RoleSet::ALL, true);

    let server = tokio::spawn(async move {
        listener
            .accept(&acceptor, SessionAssignment::Fresh(SessionId::from_u128(1)))
            .await
    });

    let mut socket = tokio::net::TcpStream::connect(addr).await.expect("dial");
    {
        let mut framed = FramedStream::new(
            &mut socket,
            FrameLimits::network(),
            ConnectionCounters::shared(),
        );
        framed
            .send_message(&ControlRequest::Hello(Hello::new(Role::Peer, token())))
            .await
            .expect("greet");
        let _: ControlReply = framed
            .expect_message(FrameKind::ControlReply)
            .await
            .expect("an answer");
    }
    let (server_conn, _channels) = server.await.expect("server task").expect("accept");

    let bytes = astrs_wire::encode_frame(
        FrameKind::PeerEvent,
        FrameFlags::CRC,
        &[7u8; 2_048],
        &FrameLimits::network(),
    )
    .expect("encode");
    socket
        .write_all(&bytes[..bytes.len() / 3])
        .await
        .expect("write");
    socket.shutdown().await.expect("shutdown");
    drop(socket);

    for _ in 0..300 {
        if server_conn.is_closed() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(server_conn.is_closed());
}

// ---------------------------------------------------------------------------
// Reconnect storm (§12)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_reconnect_storm_resumes_and_the_epochs_increment() {
    const RESTARTS: usize = 5;

    let config = TransportConfig::new().with_backoff(
        astrs_transport::BackoffConfig::new()
            .with_base(Duration::from_millis(5))
            .with_cap(Duration::from_millis(30))
            .with_jitter_percent(10),
    );

    let listener = TcpListener::bind(loopback(), config.clone())
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let acceptor = acceptor_from_config(&config, token(), RoleSet::ALL, true);

    // The server accepts, serves briefly, then drops — over and over, which is
    // exactly what a coordinator being restarted in a loop looks like.
    let server = tokio::spawn(async move {
        for index in 0..RESTARTS as u128 {
            let Ok((connection, mut channels)) = listener
                .accept(
                    &acceptor,
                    SessionAssignment::Fresh(SessionId::from_u128(index + 1)),
                )
                .await
            else {
                break;
            };
            // Drain whatever the client buffered while we were away.
            let _ = tokio::time::timeout(Duration::from_millis(80), channels.control.recv()).await;
            tokio::time::sleep(Duration::from_millis(20)).await;
            drop(connection);
        }
        listener
    });

    let dial_config = config.clone();
    let factory = move |resume: Option<SessionId>| {
        let config = dial_config.clone();
        async move {
            let params = params(&config, Role::Peer, true).with_resume(resume);
            tcp::connect(addr, &config, &params).await
        }
    };
    let (link, _channels) =
        ReconnectingConnection::spawn_with_policy(factory, config, OverflowPolicy::DropOldest);
    let mut events = link.subscribe();

    let mut ups = Vec::new();
    let mut downs = 0usize;
    let deadline = Instant::now() + GENEROUS;
    while ups.len() < RESTARTS && Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(10), events.recv()).await {
            Ok(Ok(ConnectionEvent::Up { epoch, .. })) => {
                ups.push(epoch);
                // Keep the link busy so the next incarnation has work to flush.
                let _ = link.send_control(FrameKind::DaemonEvent, b"heartbeat");
            }
            Ok(Ok(ConnectionEvent::Down { epoch, .. })) => {
                downs += 1;
                assert!(
                    ups.contains(&epoch),
                    "a Down must name an epoch that came Up first"
                );
            }
            Ok(Ok(_)) => {}
            Ok(Err(_)) | Err(_) => break,
        }
    }

    assert_eq!(
        ups.len(),
        RESTARTS,
        "the client must resume after every restart, saw {ups:?}"
    );
    assert_eq!(
        ups,
        (1..=RESTARTS as u64).collect::<Vec<_>>(),
        "epochs must increment by exactly one, monotonically"
    );
    assert!(
        downs >= RESTARTS - 1,
        "every ended incarnation reports Down"
    );

    let _ = server.await;
    link.shutdown().await;
}

#[tokio::test]
async fn frames_queued_while_down_arrive_in_order_when_the_link_returns() {
    let config = TransportConfig::new().with_backoff(
        astrs_transport::BackoffConfig::new()
            .with_base(Duration::from_millis(5))
            .with_cap(Duration::from_millis(20))
            .with_jitter_percent(0)
            .with_buffer_frames(64),
    );

    // Claim a port, then release it so nothing is listening yet.
    let probe = TcpListener::bind(loopback(), config.clone())
        .await
        .expect("bind");
    let addr = probe.local_addr().expect("local addr");
    drop(probe);

    let dial_config = config.clone();
    let factory = move |resume: Option<SessionId>| {
        let config = dial_config.clone();
        async move {
            let params = params(&config, Role::Peer, true).with_resume(resume);
            tcp::connect(addr, &config, &params).await
        }
    };
    let (link, _channels) =
        ReconnectingConnection::spawn_with_policy(factory, config.clone(), OverflowPolicy::Reject);

    for index in 0..16u8 {
        link.send_control(FrameKind::DaemonEvent, &[index])
            .expect("a send while down must queue");
    }
    assert_eq!(link.queued_frames(), 16);

    let listener = TcpListener::bind(addr, config.clone())
        .await
        .expect("rebind");
    let acceptor = acceptor_from_config(&config, token(), RoleSet::ALL, true);
    let server = tokio::spawn(async move {
        let (connection, mut channels) = listener
            .accept(&acceptor, SessionAssignment::Fresh(SessionId::from_u128(1)))
            .await
            .expect("accept");
        let mut seen = Vec::new();
        for _ in 0..16 {
            let frame = tokio::time::timeout(GENEROUS, channels.control.recv())
                .await
                .expect("no stall")
                .expect("a buffered frame");
            seen.push(frame.payload()[0]);
        }
        (connection, seen)
    });

    tokio::time::timeout(GENEROUS, link.wait_until_connected())
        .await
        .expect("no stall")
        .expect("the link must come up");

    let (_connection, seen) = server.await.expect("server task");
    assert_eq!(
        seen,
        (0..16u8).collect::<Vec<_>>(),
        "buffered frames must arrive in order"
    );
    assert_eq!(link.queued_frames(), 0);
    assert_eq!(link.dropped_frames(), 0);
    link.shutdown().await;
}

// ---------------------------------------------------------------------------
// Route-mux fairness (§6.4)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_saturated_route_does_not_delay_the_control_plane() {
    // The §6.4 promise end to end: "a camera topic never head-of-line blocks
    // a heartbeat". This is a *liveness* check — under a 2 048-frame, 8 MiB
    // flood the control plane and a second route both keep moving.
    //
    // It is deliberately not the discriminating fairness test, and it would be
    // dishonest to label it one. Over a real socket the writer drains a
    // bounded route queue faster than a producer task refills it, so the
    // backlog empties between write batches and even a scheduler with the
    // control tier deleted delivers the heartbeat promptly (verified by
    // mutation). The assertion that actually fails when `pop_next` regresses
    // is `mux::state::tests::a_control_frame_overtakes_a_deep_route_backlog`,
    // where the queue can be held reliably full.
    let burst = 4u32;
    let queue_depth = 64;
    let config = TransportConfig::new().with_mux(
        MuxConfig::new()
            .with_initial_window_frames(64)
            .with_route_queue_depth(queue_depth)
            .with_control_burst(burst),
    );
    let mut pair = tcp_pair(config).await;

    let saturated = pair.client.open_route(b"camera").expect("open");
    let quiet = pair.client.open_route(b"pose").expect("open");

    let mut saturated_server =
        tokio::time::timeout(GENEROUS, pair.server_channels.accepts.accept())
            .await
            .expect("no stall")
            .expect("an inbound route");
    let mut quiet_server = tokio::time::timeout(GENEROUS, pair.server_channels.accepts.accept())
        .await
        .expect("no stall")
        .expect("an inbound route");

    // Count route frames as they land, so the control frame's arrival can be
    // placed in that sequence.
    let route_frames = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&route_frames);
    tokio::spawn(async move {
        while saturated_server.receiver_mut().recv().await.is_some() {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    });
    let quiet_seen = Arc::new(AtomicU64::new(0));
    let quiet_counter = Arc::clone(&quiet_seen);
    tokio::spawn(async move {
        while quiet_server.receiver_mut().recv().await.is_some() {
            quiet_counter.fetch_add(1, Ordering::Relaxed);
        }
    });

    // Flood one route with a backlog far deeper than the control burst.
    const FLOOD: usize = 2_048;
    let bulk = vec![0x5au8; 4_096];
    let flood = {
        let sender = saturated.sender().clone();
        tokio::spawn(async move {
            for _ in 0..FLOOD {
                sender.send(FrameKind::Data, &bulk).await.expect("send");
            }
        })
    };

    // Let the queue fill so the control frame is genuinely queued behind it.
    let deadline = Instant::now() + Duration::from_secs(10);
    while route_frames.load(Ordering::Relaxed) < 64 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let before = route_frames.load(Ordering::Relaxed);

    let control_start = Instant::now();
    pair.client
        .open_control()
        .send(FrameKind::PeerEvent, b"heartbeat")
        .await
        .expect("send");
    let frame = tokio::time::timeout(Duration::from_secs(10), pair.server_channels.control.recv())
        .await
        .expect("the control plane must not be starved by a saturated route")
        .expect("a control frame");
    let after = route_frames.load(Ordering::Relaxed);
    let control_latency = control_start.elapsed();
    assert_eq!(frame.payload(), b"heartbeat");

    // Liveness, not ordering: the control frame must not wait behind anything
    // close to the whole backlog.
    let overtaken = after.saturating_sub(before);
    assert!(
        overtaken < (FLOOD / 4) as u64,
        "the control frame waited behind {overtaken} route frames out of a \
         {FLOOD}-frame backlog"
    );
    assert!(
        control_latency < Duration::from_secs(2),
        "control frame took {control_latency:?} behind a saturated route"
    );

    // The quiet route is not starved either.
    quiet
        .sender()
        .send(FrameKind::Data, b"a pose")
        .await
        .expect("send");
    let deadline = Instant::now() + Duration::from_secs(10);
    while quiet_seen.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        quiet_seen.load(Ordering::Relaxed),
        1,
        "the quiet route must not be starved by the saturated one"
    );

    flood.await.expect("flood task");
    assert!(route_frames.load(Ordering::Relaxed) > 0);
}

#[tokio::test]
async fn a_datagram_flood_on_a_stalled_link_stays_bounded() {
    // A datagram sender never blocks, so an unbounded queue behind a peer that
    // is not reading is an out-of-memory path. The queue must cap and drop.
    let depth = 16;
    let config = TransportConfig::new().with_mux(MuxConfig::new().with_datagram_queue_depth(depth));
    let pair = tcp_pair(config).await;

    let datagrams = pair.client.datagrams();
    let payload = vec![0xa5u8; 32 * 1024];
    for _ in 0..20_000 {
        // Never fails and never waits, whatever the link is doing.
        datagrams
            .send(FrameKind::Data, &payload)
            .expect("a datagram send must not fail on a healthy link");
    }

    let snapshot = pair.client.stats();
    assert_eq!(snapshot.connection.datagrams_sent, 20_000);
    assert!(
        snapshot.connection.datagrams_dropped > 0,
        "a 20 000-datagram flood into a {depth}-deep queue must drop"
    );
    // The bound that matters: what is still queued, not what was offered.
    assert!(
        pair.client.mux().shared().queued_datagrams() <= depth,
        "the datagram queue must never exceed its configured depth"
    );
    assert!(!pair.client.is_closed());
}

#[tokio::test]
async fn a_slow_consumer_backs_up_its_own_route_and_no_other() {
    // Flow control is per route: a consumer that stops reading must stall its
    // own producer without touching anybody else's.
    let config = TransportConfig::new().with_mux(
        MuxConfig::new()
            .with_initial_window_frames(4)
            .with_route_queue_depth(4),
    );
    let mut pair = tcp_pair(config).await;

    let stalled = pair.client.open_route(b"stalled").expect("open");
    let healthy = pair.client.open_route(b"healthy").expect("open");

    let _stalled_server = tokio::time::timeout(GENEROUS, pair.server_channels.accepts.accept())
        .await
        .expect("no stall")
        .expect("an inbound route");
    let mut healthy_server = tokio::time::timeout(GENEROUS, pair.server_channels.accepts.accept())
        .await
        .expect("no stall")
        .expect("an inbound route");

    // Fill the stalled route past its window; its sender must block.
    let sender = stalled.sender().clone();
    let blocked = tokio::spawn(async move {
        for _ in 0..64 {
            sender
                .send(FrameKind::Data, b"backing up")
                .await
                .expect("send");
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !blocked.is_finished(),
        "a consumer that never reads must stall its producer"
    );

    // …and the healthy route sails past it. The sender runs concurrently with
    // the reader on purpose: sixteen frames is four windows' worth, so a
    // send-then-receive ordering would stall on its own backpressure rather
    // than on the stalled route, and prove nothing.
    let healthy_sender = healthy.sender().clone();
    let writer = tokio::spawn(async move {
        for index in 0..16u8 {
            healthy_sender
                .send(FrameKind::Data, &[index])
                .await
                .expect("send");
        }
    });
    for index in 0..16u8 {
        let frame = tokio::time::timeout(
            Duration::from_secs(10),
            healthy_server.receiver_mut().recv(),
        )
        .await
        .expect("the healthy route must not be blocked by the stalled one")
        .expect("a route frame");
        assert_eq!(frame.payload(), &[index]);
    }
    writer.await.expect("writer task");

    // The stalled producer is still stuck, which is the point: its own
    // consumer never read, and nobody else paid for it.
    assert!(!blocked.is_finished());
    blocked.abort();
}

// ---------------------------------------------------------------------------
// Compression (§6.4, §7.1)
// ---------------------------------------------------------------------------

/// A payload that compresses well at any size.
fn compressible(len: usize) -> Vec<u8> {
    (0..len).map(|index| (index % 17) as u8).collect()
}

#[tokio::test]
async fn both_codecs_round_trip_over_a_real_socket() {
    for codec in [Compression::Lz4, Compression::Zstd] {
        let config = TransportConfig::new()
            .with_compression(CompressionPolicy::codec(codec).with_threshold_bytes(16 * 1024));
        let mut pair = tcp_pair(config).await;
        assert_eq!(pair.client.capabilities().compression, codec);

        let stream = pair.client.open_route(b"lidar").expect("open");
        let mut accepted = tokio::time::timeout(GENEROUS, pair.server_channels.accepts.accept())
            .await
            .expect("no stall")
            .expect("an inbound route");

        for len in [1usize << 16, 1 << 18] {
            let payload = compressible(len);
            stream
                .sender()
                .send(FrameKind::Data, &payload)
                .await
                .expect("send");
            let frame = tokio::time::timeout(GENEROUS, accepted.receiver_mut().recv())
                .await
                .expect("no stall")
                .expect("a route frame");
            assert_eq!(frame.payload(), payload.as_slice(), "{codec} at {len} B");
        }

        let stats = pair.client.stats();
        assert_eq!(stats.connection.frames_compressed, 2, "{codec}");
        assert!(stats.connection.compression_saved_bytes > 0, "{codec}");
    }
}

#[tokio::test]
async fn the_compression_threshold_boundary_is_exact_on_the_wire() {
    const THRESHOLD: usize = 16 * 1024;

    let config = TransportConfig::new().with_compression(
        CompressionPolicy::codec(Compression::Lz4).with_threshold_bytes(THRESHOLD),
    );
    let mut pair = tcp_pair(config).await;

    let stream = pair.client.open_route(b"boundary").expect("open");
    let mut accepted = tokio::time::timeout(GENEROUS, pair.server_channels.accepts.accept())
        .await
        .expect("no stall")
        .expect("an inbound route");

    // One byte below: raw. Exactly at, and one above: compressed.
    for (len, expect_compressed) in [
        (THRESHOLD - 1, false),
        (THRESHOLD, true),
        (THRESHOLD + 1, true),
    ] {
        let before = pair.client.stats().connection.frames_compressed;
        let payload = compressible(len);
        stream
            .sender()
            .send(FrameKind::Data, &payload)
            .await
            .expect("send");
        let frame = tokio::time::timeout(GENEROUS, accepted.receiver_mut().recv())
            .await
            .expect("no stall")
            .expect("a route frame");
        assert_eq!(frame.payload(), payload.as_slice(), "at {len} B");

        let after = pair.client.stats().connection.frames_compressed;
        assert_eq!(
            after > before,
            expect_compressed,
            "compression decision was wrong at {len} B (threshold {THRESHOLD})"
        );
    }
}

#[tokio::test]
async fn a_connection_that_negotiated_no_codec_sends_everything_raw() {
    // The client wants zstd; the acceptor advertises no codec at all. The
    // handshake intersects the feature sets, so nothing is compressed.
    let config = TransportConfig::new()
        .with_compression(CompressionPolicy::codec(Compression::Zstd).with_threshold_bytes(1_024));
    let listener = TcpListener::bind(loopback(), TransportConfig::new())
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let acceptor = acceptor_from_config(
        &TransportConfig::new().with_compression(CompressionPolicy::disabled()),
        token(),
        RoleSet::ALL,
        true,
    );

    let server = tokio::spawn(async move {
        listener
            .accept(&acceptor, SessionAssignment::Fresh(SessionId::from_u128(1)))
            .await
    });
    let (client, _cc) = tcp::connect(addr, &config, &params(&config, Role::Peer, true))
        .await
        .expect("connect");
    let (_server, mut sc) = server.await.expect("server task").expect("accept");

    assert_eq!(client.capabilities().compression, Compression::None);

    let stream = client.open_route(b"raw").expect("open");
    let mut accepted = tokio::time::timeout(GENEROUS, sc.accepts.accept())
        .await
        .expect("no stall")
        .expect("an inbound route");

    let payload = compressible(64 * 1024);
    stream
        .sender()
        .send(FrameKind::Data, &payload)
        .await
        .expect("send");
    let frame = tokio::time::timeout(GENEROUS, accepted.receiver_mut().recv())
        .await
        .expect("no stall")
        .expect("a route frame");
    assert_eq!(frame.payload(), payload.as_slice());
    assert_eq!(client.stats().connection.frames_compressed, 0);
}

// ---------------------------------------------------------------------------
// Concurrency (§6.4)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn one_hundred_routes_run_concurrently_over_a_unix_socket() {
    const ROUTES: usize = 100;
    const FRAMES: usize = 20;

    let config = TransportConfig::uds().with_mux(
        MuxConfig::new()
            .with_initial_window_frames(8)
            .with_route_queue_depth(8),
    );
    let mut pair = uds_pair("stress", config).await;

    let mut clients = Vec::with_capacity(ROUTES);
    for index in 0..ROUTES {
        clients.push(
            pair.client
                .open_route(format!("route-{index}").as_bytes())
                .expect("open"),
        );
    }

    let mut servers = Vec::with_capacity(ROUTES);
    for _ in 0..ROUTES {
        servers.push(
            tokio::time::timeout(GENEROUS, pair.server_channels.accepts.accept())
                .await
                .expect("no stall")
                .expect("an inbound route"),
        );
    }
    assert_eq!(pair.client.open_route_count(), ROUTES);
    assert_eq!(pair.server.open_route_count(), ROUTES);

    // Every route writes its own marker; every route must read exactly its own.
    let mut writers = Vec::with_capacity(ROUTES);
    for (index, stream) in clients.iter().enumerate() {
        let sender = stream.sender().clone();
        writers.push(tokio::spawn(async move {
            for sequence in 0..FRAMES {
                let payload = [(index % 251) as u8, (sequence % 251) as u8];
                sender.send(FrameKind::Data, &payload).await.expect("send");
            }
        }));
    }

    let mut readers = Vec::with_capacity(ROUTES);
    for mut stream in servers {
        readers.push(tokio::spawn(async move {
            let mut seen = 0usize;
            let mut first_byte = None;
            while seen < FRAMES {
                let frame = tokio::time::timeout(GENEROUS, stream.receiver_mut().recv())
                    .await
                    .expect("a route must not stall")
                    .expect("a route frame");
                assert_eq!(frame.payload().len(), 2);
                let tag = frame.payload()[0];
                match first_byte {
                    None => first_byte = Some(tag),
                    Some(expected) => assert_eq!(
                        tag, expected,
                        "a route received another route's frame — the demux is wrong"
                    ),
                }
                assert_eq!(
                    frame.payload()[1] as usize % 251,
                    seen % 251,
                    "frames arrived out of order on one route"
                );
                seen += 1;
            }
            seen
        }));
    }

    for writer in writers {
        writer.await.expect("writer task");
    }
    let mut total = 0usize;
    for reader in readers {
        total += reader.await.expect("reader task");
    }
    assert_eq!(total, ROUTES * FRAMES);

    let snapshot = pair.client.stats();
    assert_eq!(snapshot.routes.len(), ROUTES);
    assert_eq!(snapshot.connection.routes_active as usize, ROUTES);
    for route in snapshot.routes.values() {
        assert_eq!(route.frames_sent, FRAMES as u64);
    }
}

#[tokio::test]
async fn closing_a_connection_ends_every_route_on_it() {
    let mut pair = uds_pair("teardown", TransportConfig::uds()).await;

    let mut streams = Vec::new();
    for index in 0..8 {
        streams.push(
            pair.client
                .open_route(format!("route-{index}").as_bytes())
                .expect("open"),
        );
    }
    let mut accepted = Vec::new();
    for _ in 0..8 {
        accepted.push(
            tokio::time::timeout(GENEROUS, pair.server_channels.accepts.accept())
                .await
                .expect("no stall")
                .expect("an inbound route"),
        );
    }

    pair.client
        .close(astrs_transport::CloseReason::local("test over"))
        .await
        .expect("close");

    for mut stream in accepted {
        let ended = tokio::time::timeout(GENEROUS, stream.receiver_mut().recv())
            .await
            .expect("no stall");
        assert!(ended.is_none(), "every route must end when the link does");
    }
    for stream in &streams {
        assert!(!stream.sender().is_open());
    }

    for _ in 0..300 {
        if pair.server.is_closed() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(pair.server.is_closed());
}

// ---------------------------------------------------------------------------
// Address dispatch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_quic_address_still_reaches_a_tcp_peer_with_identical_framing() {
    // The §23 risk-5 fallback, end to end: the framing on the two planes is
    // byte-identical, so the same address dialled either way must work.
    let config = TransportConfig::new();
    let listener = TcpListener::bind(loopback(), config.clone())
        .await
        .expect("bind");
    let socket = listener.local_addr().expect("local addr");
    let acceptor = acceptor_from_config(&config, token(), RoleSet::ALL, true);

    let server = tokio::spawn(async move {
        listener
            .accept(&acceptor, SessionAssignment::Fresh(SessionId::from_u128(1)))
            .await
    });

    let (client, _cc) = astrs_transport::backend::connect_with_fallback(
        &TransportAddr::Quic(socket),
        &config,
        &params(&config, Role::Peer, true),
    )
    .await
    .expect("the fallback must reach the tcp listener");
    let (_server, mut sc) = server.await.expect("server task").expect("accept");

    assert_eq!(client.peer().plane, astrs_wire::Plane::Tcp);
    client
        .open_control()
        .send(FrameKind::PeerEvent, b"over the fallback")
        .await
        .expect("send");
    let frame = tokio::time::timeout(GENEROUS, sc.control.recv())
        .await
        .expect("no stall")
        .expect("a control frame");
    assert_eq!(frame.payload(), b"over the fallback");
}
