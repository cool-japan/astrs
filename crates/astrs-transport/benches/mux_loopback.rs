//! Peer-transport latency ladder: mux over TCP loopback (blueprint §20.4).
//!
//! | Bench | §20.4 target (0.1.0) |
//! |---|---|
//! | `transport_mux/mux_loopback_4mb` | p99 < 4 ms |
//!
//! # Setup
//!
//! `tcp_pair` below is the same wiring `tests/transport_conformance.rs`'s
//! `tcp_pair` helper uses: a real kernel TCP socket on `127.0.0.1`, port 0
//! (kernel-assigned), handshaken with [`astrs_transport::acceptor_from_config`],
//! producing one [`StreamConnection`] + [`MuxChannels`] pair per side. One
//! route is opened once and reused for every measured iteration.
//!
//! # What is timed
//!
//! Unlike the SHM handoff bench (which excludes the payload write from its
//! timed span, because writing into an already-allocated slot is a separate
//! step from publishing it), `RouteSender::send` has no such separate step —
//! framing, the mux header, the socket write and the peer's decode are the
//! *whole* operation, so the timed span is the entire `send(...).await` call
//! through to the receiver task's observed [`Instant`], reported back over a
//! [`tokio::sync::mpsc`] channel (so the channel hop itself is excluded, as
//! in the SHM bench). The payload buffer is filled once, outside the timed
//! group entirely, and reused unchanged every iteration — a `send` does not
//! need fresh bytes the way a producer publishing a fresh SHM slot does.
//!
//! This is **quiescent** end-to-end latency: the receiver task is already
//! parked in `recv().await` when each send lands (requests are not
//! pipelined — the loop waits for the ack before sending the next one), not
//! a saturated stream. See `crates/astrs-shm/benches/handoff.rs`'s header
//! for why that distinction matters for how a p99 here should be read.
//!
//! # Sampling
//!
//! [`SamplingMode::Flat`] with small, explicit `warm_up_time`/
//! `measurement_time` budgets — see the SHM handoff bench's header for why
//! criterion's own calibration otherwise over-sizes each sample for a
//! multi-millisecond-per-iteration bench like this one, and for why a
//! handful of warm-up calls may land in the reported percentiles (biasing
//! them pessimistically, never optimistically).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::cell::RefCell;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use astrs_transport::backend::tcp::{self, TcpListener};
use astrs_transport::{
    Connection, HandshakeParams, LocalIdentity, MuxChannels, StreamConnection, TransportConfig,
    acceptor_from_config,
};
use astrs_wire::{AuthToken, FrameKind, Role, RoleSet, SessionAssignment, SessionId};
use criterion::{Criterion, SamplingMode, criterion_group, criterion_main};
use tokio::sync::mpsc as tokio_mpsc;

/// Peer-transport (mux over TCP loopback), 4 MB frame, p99 (blueprint §20.4).
const MUX_TARGET: Duration = Duration::from_millis(4);

/// How long any single blocking step may take before this file declares the
/// connection stalled rather than waiting forever. Generous on purpose — see
/// `tests/transport_conformance.rs`'s identical `GENEROUS`.
const PATIENCE: Duration = Duration::from_secs(5);

/// The cluster token every fixture in this file authenticates with — the
/// same fixed value `tests/transport_conformance.rs` uses.
fn token() -> AuthToken {
    AuthToken::from_bytes([0x5e; 32])
}

/// The loopback address with an ephemeral, kernel-assigned port.
fn loopback() -> SocketAddr {
    "127.0.0.1:0".parse().expect("a valid loopback address")
}

/// Client-side handshake parameters for `role`.
fn params(config: &TransportConfig, role: Role, crc: bool) -> HandshakeParams {
    HandshakeParams::from_config(config, LocalIdentity::new(role), token(), crc)
}

/// One connected pair over a real TCP loopback socket.
struct Pair {
    client: StreamConnection,
    /// Kept alive so the connection is not torn down out from under
    /// `server_channels` — the identical idiom as `_listener` in
    /// `tests/transport_conformance.rs`'s own `Pair`. Never read past the
    /// sanity check in [`tcp_pair`].
    _server: StreamConnection,
    server_channels: MuxChannels,
}

/// Establishes a pair over a real TCP socket, wired exactly the way
/// `tests/transport_conformance.rs`'s `tcp_pair` helper wires one.
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
    let (client, _client_channels) =
        tcp::connect(addr, &config, &params(&config, Role::Peer, true))
            .await
            .expect("connect");
    let (server, server_channels) = server.await.expect("server task").expect("accept");
    assert_eq!(
        server.peer().plane,
        astrs_wire::Plane::Tcp,
        "this fixture is TCP-only"
    );

    Pair {
        client,
        _server: server,
        server_channels,
    }
}

/// Sorts `samples` in place and returns the value at percentile `p`
/// (`0.0..=100.0`), nearest-rank on the sorted sample set.
fn percentile(samples: &mut [Duration], p: f64) -> Duration {
    if samples.is_empty() {
        return Duration::ZERO;
    }
    samples.sort_unstable();
    let rank = ((p / 100.0) * (samples.len() - 1) as f64).round() as usize;
    samples[rank.min(samples.len() - 1)]
}

/// Prints the §20.4 gate line `scripts/bench-gate.sh` greps for, plus a loud
/// (but non-fatal) warning on a miss. Never fails the build itself.
fn report(name: &str, samples: &mut [Duration], target: Duration) {
    let n = samples.len();
    let p50 = percentile(samples, 50.0);
    let p99 = percentile(samples, 99.0);
    let max = samples.iter().max().copied().unwrap_or_default();
    let result = if p99 <= target { "PASS" } else { "FAIL" };
    println!(
        "BENCH_GATE name={name} n={n} p50_us={:.2} p99_us={:.2} max_us={:.2} target_us={:.2} result={result}",
        p50.as_secs_f64() * 1e6,
        p99.as_secs_f64() * 1e6,
        max.as_secs_f64() * 1e6,
        target.as_secs_f64() * 1e6,
    );
    if result == "FAIL" {
        eprintln!(
            "WARN: {name} missed its blueprint §20.4 target: p99={:.2}us > target={:.2}us (n={n} samples)",
            p99.as_secs_f64() * 1e6,
            target.as_secs_f64() * 1e6,
        );
    }
}

/// 4 MB frame, one route, one reader task, quiescent end-to-end latency
/// (see this file's header).
fn bench_mux_loopback_4mb(c: &mut Criterion) {
    const PAYLOAD_LEN: usize = 4 * 1024 * 1024;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let mut pair = runtime.block_on(tcp_pair(TransportConfig::new()));
    let (ack_tx, mut ack_rx) = tokio_mpsc::unbounded_channel::<Instant>();

    let stream = pair
        .client
        .open_route(b"bench/mux-loopback-4mb")
        .expect("open route");
    let mut accepted = runtime.block_on(async {
        tokio::time::timeout(PATIENCE, pair.server_channels.accepts.accept())
            .await
            .expect("no stall accepting the route")
            .expect("an inbound route")
    });

    let reader = runtime.spawn(async move {
        while let Some(frame) = accepted.receiver_mut().recv().await {
            let observed = Instant::now();
            drop(frame);
            if ack_tx.send(observed).is_err() {
                break;
            }
        }
    });

    let filler = vec![0xa5_u8; PAYLOAD_LEN];
    let samples = RefCell::new(Vec::<Duration>::new());

    {
        let mut group = c.benchmark_group("transport_mux");
        group.sampling_mode(SamplingMode::Flat);
        group.sample_size(10);
        group.warm_up_time(Duration::from_millis(5));
        group.measurement_time(Duration::from_millis(500));
        group.bench_function("mux_loopback_4mb", |b| {
            b.iter_custom(|iters| {
                runtime.block_on(async {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let start = Instant::now();
                        stream
                            .sender()
                            .send(FrameKind::Data, &filler)
                            .await
                            .expect("send");
                        let observed = tokio::time::timeout(PATIENCE, ack_rx.recv())
                            .await
                            .expect("no stall")
                            .expect("reader task alive");
                        let latency = observed.saturating_duration_since(start);
                        total += latency;
                        samples.borrow_mut().push(latency);
                    }
                    total
                })
            });
        });
        group.finish();
    }

    stream.close();
    runtime.block_on(async {
        let _ = tokio::time::timeout(PATIENCE, reader).await;
    });

    let mut collected = samples.into_inner();
    report("transport.mux_loopback_4mb_p99", &mut collected, MUX_TARGET);
}

criterion_group!(benches, bench_mux_loopback_4mb);
criterion_main!(benches);
