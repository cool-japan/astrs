//! The two tasks that turn a framed link into a live mux.
//!
//! A connection runs exactly two tasks, and they never contend:
//!
//! - the **writer** owns the [`FrameSink`] and drains the scheduler;
//! - the **reader** owns the [`FrameSource`] and feeds the demux.
//!
//! Two tasks rather than one `select!` loop is a deliberate simplification. A
//! single task would have to hold `&mut` on both halves and poll them together,
//! which either forces the halves into a lock or forces the loop to alternate
//! between a read and a write — turning a full-duplex link into a half-duplex
//! one. Splitting them costs one extra task per connection and buys genuine
//! concurrency in both directions.
//!
//! # Batching
//!
//! The writer takes up to [`WRITE_BATCH`] frames per pass, queues them all,
//! then flushes once. Under load that turns a burst of small control frames
//! into one `write`; when idle it is exactly one frame per flush, so latency
//! is unaffected.
//!
//! # Shutdown
//!
//! Either task ending closes the mux, which wakes the other. The reader ends on
//! end-of-stream or a fatal frame error; the writer ends when the mux closes or
//! the socket fails. Both record a [`CloseReason`], and the first one recorded
//! wins — so "the checksum failed" is what a supervisor sees, not the
//! end-of-stream that followed it.

use std::sync::Arc;

use crate::error::{CloseReason, TransportError};
use crate::framed::{FrameSink, FrameSource};

use super::state::{Dispatched, MuxShared};

/// How many frames the writer queues before it flushes.
pub const WRITE_BATCH: usize = 64;

/// Runs the outbound half of a muxed connection.
///
/// Returns when the mux closes or the sink fails. The caller normally spawns
/// this; it is public so a test can drive it inline.
pub async fn run_writer<S: FrameSink>(shared: Arc<MuxShared>, mut sink: S) {
    loop {
        let batch = shared.take_batch(WRITE_BATCH);
        if batch.is_empty() {
            if shared.is_closed() {
                break;
            }
            // `Notify` holds a permit for a `notify_one` that arrived before
            // this await, so a frame queued between `take_batch` and here
            // still wakes the writer rather than being lost.
            shared.wait_for_work().await;
            continue;
        }

        let mut written = Vec::with_capacity(batch.len());
        let mut failed = None;
        for item in batch {
            match sink.queue(&item.frame) {
                Ok(bytes) => written.push((item, bytes as u64)),
                Err(err) => {
                    // One frame this sink will not take — an oversize payload
                    // that slipped past the queue-time check, say. Drop it and
                    // keep the connection: the alternative is losing every
                    // other route to one bad frame.
                    shared.counters().record_error();
                    failed = Some(err);
                }
            }
        }

        if let Err(err) = sink.flush().await {
            shared.close(close_reason_for(&err));
            break;
        }

        for (item, wire_bytes) in written {
            sink.record_sent(wire_bytes, item.payload_bytes);
            shared.record_written(&item, wire_bytes);
            // Dropping `item` here releases its queue permit, which is what
            // lets a blocked sender proceed — after the bytes are on the wire,
            // never before.
        }

        if let Some(err) = failed
            && err.is_fatal()
        {
            shared.close(close_reason_for(&err));
            break;
        }
    }

    // Best effort: the peer may already be gone.
    let _ = sink.flush().await;
    let _ = sink.shutdown().await;
}

/// Runs the inbound half of a muxed connection.
///
/// Returns when the peer closes, the stream fails, or the demux rejects a
/// frame it cannot route.
pub async fn run_reader<S: FrameSource>(shared: Arc<MuxShared>, mut source: S) {
    loop {
        let frame = match source.recv().await {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                shared.close(CloseReason::Eof);
                break;
            }
            Err(err) => {
                shared.counters().record_error();
                shared.close(close_reason_for(&err));
                break;
            }
        };

        match shared.dispatch(frame).await {
            Ok(Dispatched::Handled) => {}
            Ok(Dispatched::PeerClosed) => {
                shared.close(CloseReason::local("receivers dropped"));
                break;
            }
            Err(err) => {
                shared.counters().record_error();
                if err.is_fatal() {
                    shared.close(close_reason_for(&err));
                    break;
                }
                // A route-scoped failure — a flow-control overrun, say — has
                // already torn down its route. The connection carries on.
            }
        }
    }
}

/// Turns a transport failure into the reason a supervisor will see.
fn close_reason_for(err: &TransportError) -> CloseReason {
    if err.is_peer_fault() {
        CloseReason::protocol(err.to_string())
    } else {
        CloseReason::transport(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::config::{CompressionPolicy, MuxConfig, Side};
    use crate::framed::{FramedReader, FramedWriter};
    use crate::mux::handle::{ControlReceiver, ControlSender, RouteStream};
    use crate::mux::header::{MuxHeader, MuxTag};
    use crate::stats::ConnectionCounters;
    use astrs_wire::{FrameKind, FrameLimits, RouteId};
    use std::time::Duration;

    /// A pair of muxes wired to each other over an in-memory duplex.
    struct Wired {
        left: Arc<MuxShared>,
        right: Arc<MuxShared>,
        left_control: ControlReceiver,
        right_control: ControlReceiver,
        left_accepts: tokio::sync::mpsc::UnboundedReceiver<crate::mux::state::InboundRoute>,
        right_accepts: tokio::sync::mpsc::UnboundedReceiver<crate::mux::state::InboundRoute>,
    }

    fn wire(config: MuxConfig, compression: CompressionPolicy) -> Wired {
        let limits = FrameLimits::uds();
        let (a, b) = tokio::io::duplex(1 << 20);
        let (a_read, a_write) = tokio::io::split(a);
        let (b_read, b_write) = tokio::io::split(b);

        let (left, left_rx) = MuxShared::new(
            config,
            Side::Initiator,
            ConnectionCounters::shared(),
            compression,
            1 << 20,
            256,
        );
        let (right, right_rx) = MuxShared::new(
            config,
            Side::Acceptor,
            ConnectionCounters::shared(),
            compression,
            1 << 20,
            256,
        );

        tokio::spawn(run_writer(
            Arc::clone(&left),
            FramedWriter::new(a_write, limits, Arc::clone(left.counters())),
        ));
        tokio::spawn(run_reader(
            Arc::clone(&left),
            FramedReader::new(a_read, limits, Arc::clone(left.counters())),
        ));
        tokio::spawn(run_writer(
            Arc::clone(&right),
            FramedWriter::new(b_write, limits, Arc::clone(right.counters())),
        ));
        tokio::spawn(run_reader(
            Arc::clone(&right),
            FramedReader::new(b_read, limits, Arc::clone(right.counters())),
        ));

        Wired {
            left,
            right,
            left_control: ControlReceiver::new(left_rx.control),
            right_control: ControlReceiver::new(right_rx.control),
            left_accepts: left_rx.accepts,
            right_accepts: right_rx.accepts,
        }
    }

    #[tokio::test]
    async fn a_control_frame_crosses_the_wire() {
        let mut wired = wire(MuxConfig::new(), CompressionPolicy::disabled());
        let sender = ControlSender::new(Arc::clone(&wired.left));
        sender.send(FrameKind::Control, b"hello").await.unwrap();

        let frame = tokio::time::timeout(Duration::from_secs(5), wired.right_control.recv())
            .await
            .unwrap()
            .expect("a control frame");
        assert_eq!(frame.kind(), FrameKind::Control);
        assert_eq!(frame.payload(), b"hello");
    }

    #[tokio::test]
    async fn a_route_opens_and_carries_payloads_both_ways() {
        let mut wired = wire(MuxConfig::new(), CompressionPolicy::disabled());
        let route = wired.left.next_route_id();
        let opened = wired.left.open_route(route, b"spec").unwrap();
        let mut client = RouteStream::from_opened(Arc::clone(&wired.left), opened);

        let inbound = tokio::time::timeout(Duration::from_secs(5), wired.right_accepts.recv())
            .await
            .unwrap()
            .expect("an inbound route");
        assert_eq!(inbound.descriptor, b"spec");
        let mut server = RouteStream::from_inbound(Arc::clone(&wired.right), inbound);
        assert_eq!(server.route(), route);

        client
            .sender()
            .send(FrameKind::PeerEvent, b"ping")
            .await
            .unwrap();
        let got = tokio::time::timeout(Duration::from_secs(5), server.receiver_mut().recv())
            .await
            .unwrap()
            .expect("a route frame");
        assert_eq!(got.payload(), b"ping");

        server
            .sender()
            .send(FrameKind::PeerEvent, b"pong")
            .await
            .unwrap();
        let got = tokio::time::timeout(Duration::from_secs(5), client.receiver_mut().recv())
            .await
            .unwrap()
            .expect("a route frame");
        assert_eq!(got.payload(), b"pong");
    }

    #[tokio::test]
    async fn credit_recycles_so_a_route_outlives_its_window() {
        let mut wired = wire(
            MuxConfig::new().with_initial_window_frames(4),
            CompressionPolicy::disabled(),
        );
        let route = wired.left.next_route_id();
        let opened = wired.left.open_route(route, b"").unwrap();
        let client = RouteStream::from_opened(Arc::clone(&wired.left), opened);
        let inbound = wired.right_accepts.recv().await.expect("an inbound route");
        let mut server = RouteStream::from_inbound(Arc::clone(&wired.right), inbound);

        let sender = client.sender().clone();
        let writer = tokio::spawn(async move {
            for index in 0..64u32 {
                sender
                    .send(FrameKind::Data, &index.to_le_bytes())
                    .await
                    .unwrap();
            }
        });

        for index in 0..64u32 {
            let frame = tokio::time::timeout(Duration::from_secs(10), server.receiver_mut().recv())
                .await
                .unwrap()
                .expect("a route frame");
            assert_eq!(frame.payload(), &index.to_le_bytes());
        }
        writer.await.unwrap();
        assert!(wired.right.counters().snapshot().credit_grants_sent > 0);
        assert!(wired.left.counters().snapshot().credit_grants_received > 0);
    }

    #[tokio::test]
    async fn a_compressed_route_round_trips() {
        let policy =
            CompressionPolicy::codec(astrs_wire::Compression::Zstd).with_threshold_bytes(64);
        let mut wired = wire(MuxConfig::new(), policy);
        let route = wired.left.next_route_id();
        let opened = wired.left.open_route(route, b"").unwrap();
        let client = RouteStream::from_opened(Arc::clone(&wired.left), opened);
        let inbound = wired.right_accepts.recv().await.expect("an inbound route");
        let mut server = RouteStream::from_inbound(Arc::clone(&wired.right), inbound);

        let payload: Vec<u8> = (0..32_768).map(|index| (index % 11) as u8).collect();
        client
            .sender()
            .send(FrameKind::Data, &payload)
            .await
            .unwrap();

        let frame = tokio::time::timeout(Duration::from_secs(10), server.receiver_mut().recv())
            .await
            .unwrap()
            .expect("a route frame");
        assert_eq!(frame.payload(), payload.as_slice());
        assert_eq!(wired.left.counters().snapshot().frames_compressed, 1);
    }

    #[tokio::test]
    async fn a_dropped_peer_closes_the_mux_with_end_of_stream() {
        let wired = wire(MuxConfig::new(), CompressionPolicy::disabled());
        wired.right.close(CloseReason::local("peer shutting down"));

        // The right-hand writer shuts its half down, which the left-hand
        // reader sees as end of stream.
        for _ in 0..200 {
            if wired.left.is_closed() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(wired.left.is_closed());
        assert_eq!(wired.left.close_reason(), Some(CloseReason::Eof));
    }

    #[tokio::test]
    async fn a_route_close_reaches_the_peer() {
        let mut wired = wire(MuxConfig::new(), CompressionPolicy::disabled());
        let route = wired.left.next_route_id();
        let opened = wired.left.open_route(route, b"").unwrap();
        let client = RouteStream::from_opened(Arc::clone(&wired.left), opened);
        let inbound = wired.right_accepts.recv().await.expect("an inbound route");
        let mut server = RouteStream::from_inbound(Arc::clone(&wired.right), inbound);

        client.close();
        let ended = tokio::time::timeout(Duration::from_secs(5), server.receiver_mut().recv())
            .await
            .unwrap();
        assert!(ended.is_none(), "the peer's receiver must end");
        assert!(!wired.right.has_route(route));
    }

    #[tokio::test]
    async fn control_frames_still_flow_while_a_route_is_saturated() {
        // The fairness guarantee of §6.4, measured rather than asserted: a
        // route with a deep backlog must not delay the control plane.
        let mut wired = wire(
            MuxConfig::new()
                .with_initial_window_frames(256)
                .with_route_queue_depth(256),
            CompressionPolicy::disabled(),
        );
        let route = wired.left.next_route_id();
        let opened = wired.left.open_route(route, b"").unwrap();
        let client = RouteStream::from_opened(Arc::clone(&wired.left), opened);
        let inbound = wired.right_accepts.recv().await.expect("an inbound route");
        let mut server = RouteStream::from_inbound(Arc::clone(&wired.right), inbound);

        // Saturate the route: 200 frames of 4 KiB queued up front.
        let bulk = vec![0x5au8; 4_096];
        for _ in 0..200 {
            client.sender().send(FrameKind::Data, &bulk).await.unwrap();
        }

        // A control frame queued behind all of that must still arrive quickly.
        let control = ControlSender::new(Arc::clone(&wired.left));
        control
            .send(FrameKind::Control, b"heartbeat")
            .await
            .unwrap();

        // Drain the route in the background so the window keeps turning over.
        tokio::spawn(async move { while server.receiver_mut().recv().await.is_some() {} });

        let frame = tokio::time::timeout(Duration::from_secs(5), wired.right_control.recv())
            .await
            .expect("the control frame must not be starved by the route")
            .expect("a control frame");
        assert_eq!(frame.payload(), b"heartbeat");
    }

    #[tokio::test]
    async fn a_hundred_routes_run_concurrently() {
        let mut wired = wire(
            MuxConfig::new().with_initial_window_frames(8),
            CompressionPolicy::disabled(),
        );

        let mut clients = Vec::new();
        for _ in 0..100 {
            let route = wired.left.next_route_id();
            let opened = wired.left.open_route(route, b"").unwrap();
            clients.push(RouteStream::from_opened(Arc::clone(&wired.left), opened));
        }

        let mut servers = Vec::new();
        for _ in 0..100 {
            let inbound = tokio::time::timeout(Duration::from_secs(10), wired.right_accepts.recv())
                .await
                .unwrap()
                .expect("an inbound route");
            servers.push(RouteStream::from_inbound(Arc::clone(&wired.right), inbound));
        }
        assert_eq!(wired.left.open_route_count(), 100);
        assert_eq!(wired.right.open_route_count(), 100);

        // Every route sends ten frames; every route must receive its own.
        let mut senders = Vec::new();
        for (index, client) in clients.iter().enumerate() {
            let sender = client.sender().clone();
            senders.push(tokio::spawn(async move {
                for sequence in 0..10u8 {
                    sender
                        .send(FrameKind::Data, &[index as u8, sequence])
                        .await
                        .unwrap();
                }
            }));
        }

        let mut readers = Vec::new();
        for (index, mut server) in servers.into_iter().enumerate() {
            readers.push(tokio::spawn(async move {
                for sequence in 0..10u8 {
                    let frame =
                        tokio::time::timeout(Duration::from_secs(30), server.receiver_mut().recv())
                            .await
                            .expect("route must not stall")
                            .expect("a route frame");
                    assert_eq!(frame.payload(), &[index as u8, sequence]);
                }
            }));
        }

        for sender in senders {
            sender.await.unwrap();
        }
        for reader in readers {
            reader.await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_corrupt_mux_header_closes_the_connection_rather_than_panicking() {
        let limits = FrameLimits::uds();
        let (a, b) = tokio::io::duplex(1 << 20);
        let (_a_read, a_write) = tokio::io::split(a);
        let (b_read, _b_write) = tokio::io::split(b);

        let (mux, _rx) = MuxShared::new(
            MuxConfig::new(),
            Side::Acceptor,
            ConnectionCounters::shared(),
            CompressionPolicy::disabled(),
            1 << 20,
            8,
        );
        tokio::spawn(run_reader(
            Arc::clone(&mux),
            FramedReader::new(b_read, limits, Arc::clone(mux.counters())),
        ));

        // Three bytes cannot be a nine-byte mux header.
        let mut raw = FramedWriter::new(a_write, limits, ConnectionCounters::shared());
        raw.send_raw(
            FrameKind::PeerEvent,
            astrs_wire::Compression::None,
            &[1, 2, 3],
        )
        .await
        .unwrap();

        for _ in 0..200 {
            if mux.is_closed() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(mux.is_closed());
        assert!(matches!(
            mux.close_reason(),
            Some(CloseReason::Protocol { .. })
        ));
    }

    #[tokio::test]
    async fn an_unroutable_frame_for_a_dead_route_does_not_kill_the_connection() {
        let mut wired = wire(MuxConfig::new(), CompressionPolicy::disabled());
        // A data frame for a route nobody opened.
        let stray = astrs_wire::Frame::new(
            FrameKind::PeerEvent,
            astrs_wire::FrameFlags::EMPTY,
            crate::mux::header::build_payload(
                MuxHeader::new(MuxTag::RouteData, RouteId::new(4_242)),
                b"stale",
            ),
        )
        .unwrap();
        let _ = wired.left.take_batch(8);
        assert_eq!(
            wired.right.dispatch(stray).await.unwrap(),
            Dispatched::Handled
        );
        assert!(!wired.right.is_closed());

        // …and the connection still works.
        let sender = ControlSender::new(Arc::clone(&wired.left));
        sender
            .send(FrameKind::Control, b"still here")
            .await
            .unwrap();
        let frame = tokio::time::timeout(Duration::from_secs(5), wired.right_control.recv())
            .await
            .unwrap()
            .expect("a control frame");
        assert_eq!(frame.payload(), b"still here");
        let _ = wired.left_control.try_recv();
        let _ = wired.left_accepts.try_recv();
    }
}
