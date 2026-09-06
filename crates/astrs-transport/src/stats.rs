//! Bandwidth and health accounting, per connection and per route.
//!
//! The daemon's metrics endpoint (§13) and `astrs top` both need to answer
//! "which route is eating the link?" without adding a lock to the hot path.
//! The counters here are therefore plain atomics updated with
//! [`Ordering::Relaxed`]: a frame write costs a handful of uncontended
//! increments, and a snapshot is a consistent-enough read of independently
//! updated values.
//!
//! *Consistent enough* is a deliberate choice, and worth being precise about.
//! A [`ConnectionStatsSnapshot`] is **not** an atomic view of one instant: the
//! `frames_sent` in it may have been read a few nanoseconds before
//! `bytes_sent`. What every counter *is*, individually, is monotone and
//! eventually exact. Metrics consume rates and totals, and neither is harmed
//! by a torn instant; a lock that made the instant exact would cost more than
//! the metric is worth.
//!
//! # Examples
//!
//! ```
//! use astrs_transport::ConnectionCounters;
//!
//! let counters = ConnectionCounters::new();
//! counters.record_frame_sent(1_024, 1_024);
//! counters.record_frame_received(512, 512);
//!
//! let snapshot = counters.snapshot();
//! assert_eq!(snapshot.frames_sent, 1);
//! assert_eq!(snapshot.wire_bytes_sent, 1_024);
//! assert_eq!(snapshot.frames_received, 1);
//! ```

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use astrs_wire::{Compression, RouteId};

/// The relaxed ordering every counter uses.
///
/// Named so the choice is visible at every call site rather than repeated as a
/// magic argument.
const RELAXED: Ordering = Ordering::Relaxed;

/// Live counters for one connection.
///
/// Cheap to clone behind an [`Arc`]; every backend shares one instance between
/// its reader half, its writer half and its public handle.
#[derive(Debug, Default)]
pub struct ConnectionCounters {
    /// Frames handed to the socket.
    frames_sent: AtomicU64,
    /// Frames pulled off the socket.
    frames_received: AtomicU64,
    /// Bytes on the wire, including headers, checksums and compression.
    wire_bytes_sent: AtomicU64,
    /// Bytes on the wire, inbound.
    wire_bytes_received: AtomicU64,
    /// Payload bytes before compression, outbound.
    payload_bytes_sent: AtomicU64,
    /// Payload bytes after decompression, inbound.
    payload_bytes_received: AtomicU64,
    /// Frames that were actually compressed.
    frames_compressed: AtomicU64,
    /// Frames that were offered to the codec and sent raw anyway.
    frames_compression_skipped: AtomicU64,
    /// Bytes the codec saved, summed over every compressed frame.
    compression_saved_bytes: AtomicU64,
    /// Datagrams sent, native or emulated.
    datagrams_sent: AtomicU64,
    /// Datagrams received.
    datagrams_received: AtomicU64,
    /// Datagrams dropped because the receive queue was full.
    datagrams_dropped: AtomicU64,
    /// Times a sender had to wait for queue space or credit.
    send_stalls: AtomicU64,
    /// Flow-control grants sent to the peer.
    credit_grants_sent: AtomicU64,
    /// Flow-control grants received from the peer.
    credit_grants_received: AtomicU64,
    /// Routes opened over this connection's lifetime.
    routes_opened: AtomicU64,
    /// Routes closed over this connection's lifetime.
    routes_closed: AtomicU64,
    /// Routes open right now.
    routes_active: AtomicU32,
    /// Errors seen on this connection.
    errors: AtomicU64,
    /// Times this connection was re-established (the reconnect epoch).
    epoch: AtomicU64,
    /// Round-trip time last measured by a `Ping`, in microseconds; zero until
    /// one completes.
    rtt_micros: AtomicU64,
    /// Backend-reported loss signal, where the backend has one.
    packets_lost: AtomicU64,
    /// Backend-reported bytes in flight, where the backend has one.
    bytes_in_flight: AtomicU64,
}

impl ConnectionCounters {
    /// A fresh set of counters, all zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A fresh set of counters behind an [`Arc`], the form backends use.
    #[must_use]
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Records one frame written to the socket.
    ///
    /// `wire_bytes` is what the socket saw; `payload_bytes` is the
    /// application's payload before any compression, so that the ratio in
    /// [`ConnectionStatsSnapshot::compression_ratio_percent`] is meaningful.
    pub fn record_frame_sent(&self, wire_bytes: u64, payload_bytes: u64) {
        self.frames_sent.fetch_add(1, RELAXED);
        self.wire_bytes_sent.fetch_add(wire_bytes, RELAXED);
        self.payload_bytes_sent.fetch_add(payload_bytes, RELAXED);
    }

    /// Records one frame read from the socket.
    pub fn record_frame_received(&self, wire_bytes: u64, payload_bytes: u64) {
        self.frames_received.fetch_add(1, RELAXED);
        self.wire_bytes_received.fetch_add(wire_bytes, RELAXED);
        self.payload_bytes_received
            .fetch_add(payload_bytes, RELAXED);
    }

    /// Records a payload the codec shrank.
    pub fn record_compressed(&self, original_bytes: u64, compressed_bytes: u64) {
        self.frames_compressed.fetch_add(1, RELAXED);
        self.compression_saved_bytes
            .fetch_add(original_bytes.saturating_sub(compressed_bytes), RELAXED);
    }

    /// Records a payload that was offered to the codec and sent raw.
    pub fn record_compression_skipped(&self) {
        self.frames_compression_skipped.fetch_add(1, RELAXED);
    }

    /// Records one datagram sent.
    pub fn record_datagram_sent(&self, wire_bytes: u64) {
        self.datagrams_sent.fetch_add(1, RELAXED);
        self.wire_bytes_sent.fetch_add(wire_bytes, RELAXED);
    }

    /// Records one datagram received.
    pub fn record_datagram_received(&self, wire_bytes: u64) {
        self.datagrams_received.fetch_add(1, RELAXED);
        self.wire_bytes_received.fetch_add(wire_bytes, RELAXED);
    }

    /// Records one datagram dropped for want of queue space.
    pub fn record_datagram_dropped(&self) {
        self.datagrams_dropped.fetch_add(1, RELAXED);
    }

    /// Records a sender that had to wait.
    pub fn record_send_stall(&self) {
        self.send_stalls.fetch_add(1, RELAXED);
    }

    /// Records a flow-control grant sent to the peer.
    pub fn record_credit_granted(&self) {
        self.credit_grants_sent.fetch_add(1, RELAXED);
    }

    /// Records a flow-control grant received from the peer.
    pub fn record_credit_received(&self) {
        self.credit_grants_received.fetch_add(1, RELAXED);
    }

    /// Records a route opening.
    pub fn record_route_opened(&self) {
        self.routes_opened.fetch_add(1, RELAXED);
        self.routes_active.fetch_add(1, RELAXED);
    }

    /// Records a route closing.
    ///
    /// Saturates at zero rather than wrapping if a route is somehow closed
    /// twice: a metric that reads `4294967295 routes open` is worse than one
    /// that reads zero.
    pub fn record_route_closed(&self) {
        self.routes_closed.fetch_add(1, RELAXED);
        let _ = self
            .routes_active
            .fetch_update(RELAXED, RELAXED, |active| Some(active.saturating_sub(1)));
    }

    /// Records an error seen on this connection.
    pub fn record_error(&self) {
        self.errors.fetch_add(1, RELAXED);
    }

    /// Advances the reconnect epoch and returns the new value.
    ///
    /// The epoch is the connection's incarnation number: every successful
    /// re-establishment increments it, so a consumer that saw epoch `n` knows
    /// that anything it queued before is from an older link.
    pub fn bump_epoch(&self) -> u64 {
        self.epoch.fetch_add(1, RELAXED) + 1
    }

    /// The current reconnect epoch.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch.load(RELAXED)
    }

    /// Stamps the incarnation number onto these counters.
    ///
    /// A connection's counters are created fresh for each incarnation, so they
    /// cannot *derive* the epoch — the supervisor that owns the reconnect loop
    /// knows it and writes it here, so that
    /// [`ConnectionStatsSnapshot::epoch`] agrees with
    /// [`crate::ReconnectingConnection::epoch`] instead of reading zero
    /// forever.
    pub fn set_epoch(&self, epoch: u64) {
        self.epoch.store(epoch, RELAXED);
    }

    /// Records a measured round trip.
    pub fn record_rtt_micros(&self, micros: u64) {
        self.rtt_micros.store(micros, RELAXED);
    }

    /// Records backend-reported path statistics.
    ///
    /// Only QUIC has these; the stream backends leave them at zero, and the
    /// snapshot reports [`None`] rather than a misleading `0`.
    pub fn record_path_stats(&self, packets_lost: u64, bytes_in_flight: u64) {
        self.packets_lost.store(packets_lost, RELAXED);
        self.bytes_in_flight.store(bytes_in_flight, RELAXED);
    }

    /// Reads every counter into a plain struct.
    #[must_use]
    pub fn snapshot(&self) -> ConnectionStatsSnapshot {
        let packets_lost = self.packets_lost.load(RELAXED);
        let bytes_in_flight = self.bytes_in_flight.load(RELAXED);
        let rtt_micros = self.rtt_micros.load(RELAXED);
        ConnectionStatsSnapshot {
            frames_sent: self.frames_sent.load(RELAXED),
            frames_received: self.frames_received.load(RELAXED),
            wire_bytes_sent: self.wire_bytes_sent.load(RELAXED),
            wire_bytes_received: self.wire_bytes_received.load(RELAXED),
            payload_bytes_sent: self.payload_bytes_sent.load(RELAXED),
            payload_bytes_received: self.payload_bytes_received.load(RELAXED),
            frames_compressed: self.frames_compressed.load(RELAXED),
            frames_compression_skipped: self.frames_compression_skipped.load(RELAXED),
            compression_saved_bytes: self.compression_saved_bytes.load(RELAXED),
            datagrams_sent: self.datagrams_sent.load(RELAXED),
            datagrams_received: self.datagrams_received.load(RELAXED),
            datagrams_dropped: self.datagrams_dropped.load(RELAXED),
            send_stalls: self.send_stalls.load(RELAXED),
            credit_grants_sent: self.credit_grants_sent.load(RELAXED),
            credit_grants_received: self.credit_grants_received.load(RELAXED),
            routes_opened: self.routes_opened.load(RELAXED),
            routes_closed: self.routes_closed.load(RELAXED),
            routes_active: self.routes_active.load(RELAXED),
            errors: self.errors.load(RELAXED),
            epoch: self.epoch.load(RELAXED),
            rtt_micros: (rtt_micros > 0).then_some(rtt_micros),
            path: (packets_lost > 0 || bytes_in_flight > 0).then_some(PathStats {
                packets_lost,
                bytes_in_flight,
            }),
        }
    }
}

/// Live counters for one route.
///
/// Deliberately a subset of [`ConnectionCounters`]: a route has no handshake,
/// no epoch and no path statistics of its own, and duplicating fields that can
/// never differ would only invite them to drift.
#[derive(Debug, Default)]
pub struct RouteCounters {
    /// Frames queued for this route.
    frames_sent: AtomicU64,
    /// Frames delivered from this route.
    frames_received: AtomicU64,
    /// Payload bytes sent, before compression.
    payload_bytes_sent: AtomicU64,
    /// Payload bytes received, after decompression.
    payload_bytes_received: AtomicU64,
    /// Wire bytes sent for this route, after compression and framing.
    wire_bytes_sent: AtomicU64,
    /// Wire bytes received for this route.
    wire_bytes_received: AtomicU64,
    /// Frames compressed on this route.
    frames_compressed: AtomicU64,
    /// Bytes the codec saved on this route.
    compression_saved_bytes: AtomicU64,
    /// Times a sender waited for this route's window.
    send_stalls: AtomicU64,
    /// Frames dropped because the peer overran its window.
    frames_dropped: AtomicU64,
    /// Credit remaining, in frames.
    credit_remaining: AtomicU32,
}

impl RouteCounters {
    /// A fresh set of counters with `window` frames of credit.
    #[must_use]
    pub fn new(window: u32) -> Self {
        let counters = Self::default();
        counters.credit_remaining.store(window, RELAXED);
        counters
    }

    /// A fresh set of counters behind an [`Arc`].
    #[must_use]
    pub fn shared(window: u32) -> Arc<Self> {
        Arc::new(Self::new(window))
    }

    /// Records one frame queued for this route.
    pub fn record_frame_sent(&self, wire_bytes: u64, payload_bytes: u64) {
        self.frames_sent.fetch_add(1, RELAXED);
        self.wire_bytes_sent.fetch_add(wire_bytes, RELAXED);
        self.payload_bytes_sent.fetch_add(payload_bytes, RELAXED);
    }

    /// Records one frame delivered from this route.
    pub fn record_frame_received(&self, wire_bytes: u64, payload_bytes: u64) {
        self.frames_received.fetch_add(1, RELAXED);
        self.wire_bytes_received.fetch_add(wire_bytes, RELAXED);
        self.payload_bytes_received
            .fetch_add(payload_bytes, RELAXED);
    }

    /// Records a compressed frame on this route.
    pub fn record_compressed(&self, original_bytes: u64, compressed_bytes: u64) {
        self.frames_compressed.fetch_add(1, RELAXED);
        self.compression_saved_bytes
            .fetch_add(original_bytes.saturating_sub(compressed_bytes), RELAXED);
    }

    /// Records a sender that waited for this route's window.
    pub fn record_send_stall(&self) {
        self.send_stalls.fetch_add(1, RELAXED);
    }

    /// Records a frame dropped for want of receive capacity.
    pub fn record_frame_dropped(&self) {
        self.frames_dropped.fetch_add(1, RELAXED);
    }

    /// Publishes the current credit, for the snapshot.
    pub fn set_credit_remaining(&self, credit: u32) {
        self.credit_remaining.store(credit, RELAXED);
    }

    /// Reads every counter into a plain struct.
    #[must_use]
    pub fn snapshot(&self, route: RouteId) -> RouteStatsSnapshot {
        RouteStatsSnapshot {
            route,
            frames_sent: self.frames_sent.load(RELAXED),
            frames_received: self.frames_received.load(RELAXED),
            wire_bytes_sent: self.wire_bytes_sent.load(RELAXED),
            wire_bytes_received: self.wire_bytes_received.load(RELAXED),
            payload_bytes_sent: self.payload_bytes_sent.load(RELAXED),
            payload_bytes_received: self.payload_bytes_received.load(RELAXED),
            frames_compressed: self.frames_compressed.load(RELAXED),
            compression_saved_bytes: self.compression_saved_bytes.load(RELAXED),
            send_stalls: self.send_stalls.load(RELAXED),
            frames_dropped: self.frames_dropped.load(RELAXED),
            credit_remaining: self.credit_remaining.load(RELAXED),
            compression: Compression::None,
        }
    }
}

/// Backend-reported path health, where the backend measures it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct PathStats {
    /// Packets the congestion controller declared lost.
    pub packets_lost: u64,
    /// Bytes sent but not yet acknowledged.
    pub bytes_in_flight: u64,
}

/// A point-in-time reading of one connection's counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct ConnectionStatsSnapshot {
    /// Frames handed to the socket.
    pub frames_sent: u64,
    /// Frames pulled off the socket.
    pub frames_received: u64,
    /// Bytes on the wire, outbound.
    pub wire_bytes_sent: u64,
    /// Bytes on the wire, inbound.
    pub wire_bytes_received: u64,
    /// Payload bytes before compression, outbound.
    pub payload_bytes_sent: u64,
    /// Payload bytes after decompression, inbound.
    pub payload_bytes_received: u64,
    /// Frames the codec shrank.
    pub frames_compressed: u64,
    /// Frames offered to the codec and sent raw.
    pub frames_compression_skipped: u64,
    /// Bytes the codec saved.
    pub compression_saved_bytes: u64,
    /// Datagrams sent.
    pub datagrams_sent: u64,
    /// Datagrams received.
    pub datagrams_received: u64,
    /// Datagrams dropped for want of queue space.
    pub datagrams_dropped: u64,
    /// Times a sender waited.
    pub send_stalls: u64,
    /// Flow-control grants sent.
    pub credit_grants_sent: u64,
    /// Flow-control grants received.
    pub credit_grants_received: u64,
    /// Routes opened over this connection's lifetime.
    pub routes_opened: u64,
    /// Routes closed over this connection's lifetime.
    pub routes_closed: u64,
    /// Routes open right now.
    pub routes_active: u32,
    /// Errors seen.
    pub errors: u64,
    /// The connection incarnation.
    pub epoch: u64,
    /// The last measured round trip, in microseconds.
    pub rtt_micros: Option<u64>,
    /// Backend-reported path health.
    pub path: Option<PathStats>,
}

impl ConnectionStatsSnapshot {
    /// Bytes on the wire in both directions.
    #[must_use]
    pub const fn wire_bytes_total(&self) -> u64 {
        self.wire_bytes_sent
            .saturating_add(self.wire_bytes_received)
    }

    /// Frames in both directions.
    #[must_use]
    pub const fn frames_total(&self) -> u64 {
        self.frames_sent.saturating_add(self.frames_received)
    }

    /// Outbound wire bytes as a percentage of the payload they carried.
    ///
    /// Below 100 means compression is winning; above means the framing
    /// overhead of many small frames dominates. [`None`] when nothing has been
    /// sent, because a ratio with a zero denominator is not "100%", it is
    /// "no data".
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_transport::ConnectionCounters;
    ///
    /// let counters = ConnectionCounters::new();
    /// assert_eq!(counters.snapshot().compression_ratio_percent(), None);
    ///
    /// counters.record_frame_sent(500, 1_000);
    /// assert_eq!(counters.snapshot().compression_ratio_percent(), Some(50));
    /// ```
    #[must_use]
    pub const fn compression_ratio_percent(&self) -> Option<u32> {
        if self.payload_bytes_sent == 0 {
            return None;
        }
        let ratio = self.wire_bytes_sent.saturating_mul(100) / self.payload_bytes_sent;
        Some(if ratio > u32::MAX as u64 {
            u32::MAX
        } else {
            ratio as u32
        })
    }

    /// The difference between this snapshot and an earlier one.
    ///
    /// Cumulative counters are what the transport keeps; rates are what a
    /// dashboard wants. Subtracting saturates, so a snapshot taken across a
    /// counter reset reports zero rather than a nonsense spike.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_transport::ConnectionCounters;
    ///
    /// let counters = ConnectionCounters::new();
    /// counters.record_frame_sent(100, 100);
    /// let first = counters.snapshot();
    /// counters.record_frame_sent(100, 100);
    /// let delta = counters.snapshot().since(&first);
    /// assert_eq!(delta.frames_sent, 1);
    /// ```
    #[must_use]
    pub const fn since(&self, earlier: &Self) -> Self {
        Self {
            frames_sent: self.frames_sent.saturating_sub(earlier.frames_sent),
            frames_received: self.frames_received.saturating_sub(earlier.frames_received),
            wire_bytes_sent: self.wire_bytes_sent.saturating_sub(earlier.wire_bytes_sent),
            wire_bytes_received: self
                .wire_bytes_received
                .saturating_sub(earlier.wire_bytes_received),
            payload_bytes_sent: self
                .payload_bytes_sent
                .saturating_sub(earlier.payload_bytes_sent),
            payload_bytes_received: self
                .payload_bytes_received
                .saturating_sub(earlier.payload_bytes_received),
            frames_compressed: self
                .frames_compressed
                .saturating_sub(earlier.frames_compressed),
            frames_compression_skipped: self
                .frames_compression_skipped
                .saturating_sub(earlier.frames_compression_skipped),
            compression_saved_bytes: self
                .compression_saved_bytes
                .saturating_sub(earlier.compression_saved_bytes),
            datagrams_sent: self.datagrams_sent.saturating_sub(earlier.datagrams_sent),
            datagrams_received: self
                .datagrams_received
                .saturating_sub(earlier.datagrams_received),
            datagrams_dropped: self
                .datagrams_dropped
                .saturating_sub(earlier.datagrams_dropped),
            send_stalls: self.send_stalls.saturating_sub(earlier.send_stalls),
            credit_grants_sent: self
                .credit_grants_sent
                .saturating_sub(earlier.credit_grants_sent),
            credit_grants_received: self
                .credit_grants_received
                .saturating_sub(earlier.credit_grants_received),
            routes_opened: self.routes_opened.saturating_sub(earlier.routes_opened),
            routes_closed: self.routes_closed.saturating_sub(earlier.routes_closed),
            // Gauges are not differences: report the current value.
            routes_active: self.routes_active,
            errors: self.errors.saturating_sub(earlier.errors),
            epoch: self.epoch,
            rtt_micros: self.rtt_micros,
            path: self.path,
        }
    }
}

/// A point-in-time reading of one route's counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct RouteStatsSnapshot {
    /// Which route this describes.
    pub route: RouteId,
    /// Frames queued for this route.
    pub frames_sent: u64,
    /// Frames delivered from this route.
    pub frames_received: u64,
    /// Wire bytes sent for this route.
    pub wire_bytes_sent: u64,
    /// Wire bytes received for this route.
    pub wire_bytes_received: u64,
    /// Payload bytes sent, before compression.
    pub payload_bytes_sent: u64,
    /// Payload bytes received, after decompression.
    pub payload_bytes_received: u64,
    /// Frames compressed on this route.
    pub frames_compressed: u64,
    /// Bytes the codec saved on this route.
    pub compression_saved_bytes: u64,
    /// Times a sender waited for this route's window.
    pub send_stalls: u64,
    /// Frames dropped because the peer overran its window.
    pub frames_dropped: u64,
    /// Credit remaining, in frames.
    pub credit_remaining: u32,
    /// The codec negotiated for this route.
    pub compression: Compression,
}

impl RouteStatsSnapshot {
    /// The route's codec, attached after the counters were read.
    #[must_use]
    pub const fn with_compression(mut self, compression: Compression) -> Self {
        self.compression = compression;
        self
    }

    /// Whether this route currently has room to send.
    #[must_use]
    pub const fn has_credit(&self) -> bool {
        self.credit_remaining > 0
    }
}

/// Everything the daemon's metrics endpoint reads from one connection.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TransportSnapshot {
    /// The connection's own counters.
    pub connection: ConnectionStatsSnapshot,
    /// One entry per open route, in handle order.
    pub routes: BTreeMap<RouteId, RouteStatsSnapshot>,
}

impl TransportSnapshot {
    /// A snapshot with no routes.
    #[must_use]
    pub fn new(connection: ConnectionStatsSnapshot) -> Self {
        Self {
            connection,
            routes: BTreeMap::new(),
        }
    }

    /// Attaches the per-route readings.
    #[must_use]
    pub fn with_routes(mut self, routes: impl IntoIterator<Item = RouteStatsSnapshot>) -> Self {
        self.routes = routes
            .into_iter()
            .map(|snapshot| (snapshot.route, snapshot))
            .collect();
        self
    }

    /// The route that has sent the most wire bytes, if any has.
    ///
    /// This is the question an operator staring at a saturated link actually
    /// asks, so the answer is one call rather than a fold at every call site.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_transport::{ConnectionCounters, RouteCounters, TransportSnapshot};
    /// use astrs_wire::RouteId;
    ///
    /// let quiet = RouteCounters::new(8);
    /// quiet.record_frame_sent(10, 10);
    /// let loud = RouteCounters::new(8);
    /// loud.record_frame_sent(10_000, 10_000);
    ///
    /// let snapshot = TransportSnapshot::new(ConnectionCounters::new().snapshot())
    ///     .with_routes([
    ///         quiet.snapshot(RouteId::new(1)),
    ///         loud.snapshot(RouteId::new(2)),
    ///     ]);
    /// assert_eq!(snapshot.busiest_route().map(|r| r.route), Some(RouteId::new(2)));
    /// ```
    #[must_use]
    pub fn busiest_route(&self) -> Option<&RouteStatsSnapshot> {
        self.routes
            .values()
            .max_by_key(|route| route.wire_bytes_sent)
            .filter(|route| route.wire_bytes_sent > 0)
    }

    /// Wire bytes sent across every route, excluding control traffic.
    #[must_use]
    pub fn route_wire_bytes_sent(&self) -> u64 {
        self.routes.values().fold(0u64, |total, route| {
            total.saturating_add(route.wire_bytes_sent)
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn a_fresh_snapshot_is_all_zero() {
        let snapshot = ConnectionCounters::new().snapshot();
        assert_eq!(snapshot, ConnectionStatsSnapshot::default());
        assert_eq!(snapshot.frames_total(), 0);
        assert_eq!(snapshot.wire_bytes_total(), 0);
        assert_eq!(snapshot.rtt_micros, None);
        assert_eq!(snapshot.path, None);
    }

    #[test]
    fn traffic_accumulates_in_both_directions() {
        let counters = ConnectionCounters::new();
        counters.record_frame_sent(120, 100);
        counters.record_frame_sent(120, 100);
        counters.record_frame_received(60, 50);

        let snapshot = counters.snapshot();
        assert_eq!(snapshot.frames_sent, 2);
        assert_eq!(snapshot.frames_received, 1);
        assert_eq!(snapshot.wire_bytes_sent, 240);
        assert_eq!(snapshot.wire_bytes_received, 60);
        assert_eq!(snapshot.payload_bytes_sent, 200);
        assert_eq!(snapshot.frames_total(), 3);
        assert_eq!(snapshot.wire_bytes_total(), 300);
    }

    #[test]
    fn the_compression_ratio_needs_a_denominator() {
        let counters = ConnectionCounters::new();
        assert_eq!(counters.snapshot().compression_ratio_percent(), None);
        counters.record_frame_sent(250, 1_000);
        assert_eq!(counters.snapshot().compression_ratio_percent(), Some(25));
    }

    #[test]
    fn compression_savings_never_go_negative() {
        let counters = ConnectionCounters::new();
        // A codec that expanded the payload must not underflow the saving.
        counters.record_compressed(100, 150);
        assert_eq!(counters.snapshot().compression_saved_bytes, 0);
        counters.record_compressed(1_000, 400);
        assert_eq!(counters.snapshot().compression_saved_bytes, 600);
        counters.record_compression_skipped();
        assert_eq!(counters.snapshot().frames_compression_skipped, 1);
    }

    #[test]
    fn the_active_route_gauge_saturates_at_zero() {
        let counters = ConnectionCounters::new();
        counters.record_route_opened();
        counters.record_route_opened();
        assert_eq!(counters.snapshot().routes_active, 2);
        counters.record_route_closed();
        counters.record_route_closed();
        counters.record_route_closed();
        let snapshot = counters.snapshot();
        assert_eq!(snapshot.routes_active, 0);
        assert_eq!(snapshot.routes_opened, 2);
        assert_eq!(snapshot.routes_closed, 3);
    }

    #[test]
    fn the_epoch_starts_at_zero_and_counts_reconnects() {
        let counters = ConnectionCounters::new();
        assert_eq!(counters.epoch(), 0);
        assert_eq!(counters.bump_epoch(), 1);
        assert_eq!(counters.bump_epoch(), 2);
        assert_eq!(counters.snapshot().epoch, 2);

        // A supervisor with its own incarnation counter stamps it directly.
        counters.set_epoch(9);
        assert_eq!(counters.epoch(), 9);
        assert_eq!(counters.snapshot().epoch, 9);
    }

    #[test]
    fn path_stats_stay_absent_until_a_backend_reports_them() {
        let counters = ConnectionCounters::new();
        assert_eq!(counters.snapshot().path, None);
        counters.record_path_stats(3, 4_096);
        assert_eq!(
            counters.snapshot().path,
            Some(PathStats {
                packets_lost: 3,
                bytes_in_flight: 4_096,
            })
        );
        counters.record_rtt_micros(1_500);
        assert_eq!(counters.snapshot().rtt_micros, Some(1_500));
    }

    #[test]
    fn datagram_and_stall_counters_move() {
        let counters = ConnectionCounters::new();
        counters.record_datagram_sent(64);
        counters.record_datagram_received(32);
        counters.record_datagram_dropped();
        counters.record_send_stall();
        counters.record_credit_granted();
        counters.record_credit_received();
        counters.record_error();

        let snapshot = counters.snapshot();
        assert_eq!(snapshot.datagrams_sent, 1);
        assert_eq!(snapshot.datagrams_received, 1);
        assert_eq!(snapshot.datagrams_dropped, 1);
        assert_eq!(snapshot.send_stalls, 1);
        assert_eq!(snapshot.credit_grants_sent, 1);
        assert_eq!(snapshot.credit_grants_received, 1);
        assert_eq!(snapshot.errors, 1);
        // Datagram bytes count towards the wire totals.
        assert_eq!(snapshot.wire_bytes_sent, 64);
        assert_eq!(snapshot.wire_bytes_received, 32);
    }

    #[test]
    fn a_delta_subtracts_counters_and_carries_gauges() {
        let counters = ConnectionCounters::new();
        counters.record_frame_sent(100, 100);
        counters.record_route_opened();
        let first = counters.snapshot();

        counters.record_frame_sent(100, 100);
        counters.record_frame_received(50, 50);
        counters.record_route_opened();
        let delta = counters.snapshot().since(&first);

        assert_eq!(delta.frames_sent, 1);
        assert_eq!(delta.frames_received, 1);
        assert_eq!(delta.wire_bytes_sent, 100);
        assert_eq!(delta.routes_opened, 1);
        // The gauge is not a difference.
        assert_eq!(delta.routes_active, 2);
    }

    #[test]
    fn a_delta_against_a_larger_snapshot_saturates() {
        let counters = ConnectionCounters::new();
        counters.record_frame_sent(100, 100);
        let later = counters.snapshot();
        let delta = ConnectionStatsSnapshot::default().since(&later);
        assert_eq!(delta.frames_sent, 0);
        assert_eq!(delta.wire_bytes_sent, 0);
    }

    #[test]
    fn route_counters_track_their_own_slice() {
        let counters = RouteCounters::new(16);
        counters.record_frame_sent(120, 100);
        counters.record_frame_received(60, 50);
        counters.record_compressed(100, 40);
        counters.record_send_stall();
        counters.record_frame_dropped();
        counters.set_credit_remaining(12);

        let snapshot = counters
            .snapshot(RouteId::new(9))
            .with_compression(Compression::Zstd);
        assert_eq!(snapshot.route, RouteId::new(9));
        assert_eq!(snapshot.frames_sent, 1);
        assert_eq!(snapshot.frames_received, 1);
        assert_eq!(snapshot.compression_saved_bytes, 60);
        assert_eq!(snapshot.send_stalls, 1);
        assert_eq!(snapshot.frames_dropped, 1);
        assert_eq!(snapshot.credit_remaining, 12);
        assert_eq!(snapshot.compression, Compression::Zstd);
        assert!(snapshot.has_credit());
        counters.set_credit_remaining(0);
        assert!(!counters.snapshot(RouteId::new(9)).has_credit());
    }

    #[test]
    fn a_transport_snapshot_finds_the_busiest_route() {
        let quiet = RouteCounters::new(8);
        quiet.record_frame_sent(10, 10);
        let loud = RouteCounters::new(8);
        loud.record_frame_sent(10_000, 10_000);

        let snapshot = TransportSnapshot::new(ConnectionCounters::new().snapshot()).with_routes([
            quiet.snapshot(RouteId::new(1)),
            loud.snapshot(RouteId::new(2)),
        ]);
        assert_eq!(snapshot.routes.len(), 2);
        assert_eq!(
            snapshot.busiest_route().map(|route| route.route),
            Some(RouteId::new(2))
        );
        assert_eq!(snapshot.route_wire_bytes_sent(), 10_010);
    }

    #[test]
    fn an_idle_connection_has_no_busiest_route() {
        let idle = RouteCounters::new(8);
        let snapshot = TransportSnapshot::new(ConnectionCounters::new().snapshot())
            .with_routes([idle.snapshot(RouteId::new(1))]);
        assert!(snapshot.busiest_route().is_none());
        assert_eq!(snapshot.route_wire_bytes_sent(), 0);
    }

    #[test]
    fn counters_are_safe_to_share_across_tasks() {
        let counters = ConnectionCounters::shared();
        let mut handles = Vec::new();
        for _ in 0..8 {
            let counters = Arc::clone(&counters);
            handles.push(std::thread::spawn(move || {
                for _ in 0..1_000 {
                    counters.record_frame_sent(10, 10);
                }
            }));
        }
        for handle in handles {
            handle.join().expect("worker thread");
        }
        let snapshot = counters.snapshot();
        assert_eq!(snapshot.frames_sent, 8_000);
        assert_eq!(snapshot.wire_bytes_sent, 80_000);
    }

    #[test]
    fn route_counters_are_shareable_too() {
        let counters = RouteCounters::shared(4);
        assert_eq!(counters.snapshot(RouteId::FIRST).credit_remaining, 4);
        assert_eq!(
            Arc::clone(&counters).snapshot(RouteId::FIRST).frames_sent,
            0
        );
    }
}
