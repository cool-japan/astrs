//! [`BeaconWatcher`] — turns inbound beacons into a stream of
//! [`BeaconEvent`]s (blueprint §6.4: "`BeaconWatcher` -> stream of
//! `PeerEvent` { Discovered, Refreshed, Lost(timeout 3x interval) } with
//! per-peer HLC dedup"; named [`BeaconEvent`] rather than the task
//! description's literal `PeerEvent` to avoid colliding with the unrelated,
//! already-frozen [`astrs_wire::PeerEvent`] — see the crate root docs'
//! "Naming" section).
//!
//! The state machine ([`PeerTable`]) is a plain, synchronous data
//! structure with no socket, no `tokio`, and no wall-clock reads of its
//! own — every method takes the current [`std::time::Instant`] as a
//! parameter — so its dedup, precedence and timeout logic is unit-tested
//! directly, by constructing `Instant`s (optionally through
//! [`astrs_time::ManualClock`] for readable elapsed-time arithmetic) and
//! feeding it beacons by hand. [`BeaconWatcher`] is the thin async loop
//! wrapped around it, generic over [`crate::socket::DiscoverySocket`] so
//! *that* loop — not just the state machine underneath it — is itself
//! testable against an in-memory fake socket with no real network
//! involved.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use astrs_wire::{AuthToken, DaemonId};
use tokio::sync::mpsc;

use crate::beacon::{Beacon, BeaconRole};
use crate::defaults::{DEFAULT_BEACON_INTERVAL, DEFAULT_LOSS_MULTIPLIER, RECV_BUFFER_LEN};
use crate::socket::DiscoverySocket;

use astrs_time::HlcTimestamp;

/// A snapshot of what the cluster currently knows about one peer, carried
/// by every [`BeaconEvent`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    /// The peer's per-process identity.
    pub id: DaemonId,
    /// The role it announced.
    pub role: BeaconRole,
    /// The addresses it announced reaching it at.
    pub listen_addrs: Vec<SocketAddr>,
    /// The protocol version it announced.
    pub protocol: u16,
    /// The peer's own HLC timestamp as of its most recent beacon that
    /// actually changed this snapshot (a deduplicated repeat does not
    /// advance this field — see [`PeerTable::observe`]).
    pub hlc: HlcTimestamp,
    /// The address its most recent beacon (deduplicated or not) physically
    /// arrived from. Unlike `listen_addrs` (what the peer *claims*), this
    /// is what the watcher's own socket actually observed, and is always
    /// kept current.
    pub source: SocketAddr,
}

/// An update to the watcher's knowledge of the cluster's peers (blueprint
/// §6.4).
///
/// `#[non_exhaustive]`: a future variant (e.g. distinguishing a
/// version-mismatched peer) is added at the tail.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BeaconEvent {
    /// A peer identity not previously known was observed.
    Discovered(PeerInfo),
    /// A previously known peer sent a beacon whose HLC strictly advanced
    /// past its last recorded one, or whose announced role/addresses
    /// changed (see [`PeerTable::observe`]'s docs for why the latter
    /// matters even without an HLC advance).
    Refreshed(PeerInfo),
    /// A previously known peer has not been heard from for at least the
    /// configured loss timeout. Carries the last snapshot recorded before
    /// the timeout fired.
    Lost(PeerInfo),
}

impl BeaconEvent {
    /// The [`PeerInfo`] carried by any variant.
    #[must_use]
    pub const fn info(&self) -> &PeerInfo {
        match self {
            Self::Discovered(info) | Self::Refreshed(info) | Self::Lost(info) => info,
        }
    }
}

/// One tracked peer's bookkeeping: its latest [`PeerInfo`] plus when it was
/// last heard from at all (including deduplicated repeats).
struct PeerRecord {
    info: PeerInfo,
    last_seen: Instant,
}

/// The pure peer liveness state machine behind [`BeaconWatcher`].
///
/// # Dedup rule
///
/// A beacon from a known peer only produces a [`BeaconEvent::Refreshed`] (and
/// only updates the recorded `role`/`listen_addrs`/`hlc`) if its HLC is
/// **strictly greater** than the last one recorded for that peer, or if its
/// `role`/`listen_addrs` differ from what is currently recorded. The first
/// condition is the common case: it collapses duplicate delivery (multicast
/// fan-out routinely double-delivers a datagram at the network layer) into
/// silence rather than a spurious `Refreshed`. The second condition is a
/// deliberate escape hatch: [`crate::beacon::Beacon`]'s identity contract
/// asks every caller to mint a fresh [`DaemonId`] per process start
/// specifically so a restarted peer's HLC is never behind its own past
/// self — but a hypothetical caller that violates that contract (reuses a
/// `DaemonId` across a restart) would otherwise wedge a `PeerInfo` that no
/// longer matches reality behind a frozen, already-seen HLC forever. Either
/// condition alone is sufficient; a beacon that changes nothing observable
/// is dropped without producing an event, but — critically — still counts
/// as proof of life: `last_seen` (and therefore the [`PeerTable::sweep`]
/// timeout clock) advances on *every* validated beacon, deduplicated or
/// not.
pub struct PeerTable {
    peers: HashMap<DaemonId, PeerRecord>,
    loss_timeout: Duration,
}

impl PeerTable {
    /// Builds an empty table with the given loss timeout (see
    /// [`WatcherConfig::loss_timeout`]).
    #[must_use]
    pub fn new(loss_timeout: Duration) -> Self {
        Self {
            peers: HashMap::new(),
            loss_timeout,
        }
    }

    /// The configured loss timeout.
    #[must_use]
    pub const fn loss_timeout(&self) -> Duration {
        self.loss_timeout
    }

    /// How many peers are currently tracked (not yet [`BeaconEvent::Lost`]).
    #[must_use]
    pub fn len(&self) -> usize {
        self.peers.len()
    }

    /// Whether no peers are currently tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// The current snapshot for `id`, if it is being tracked.
    #[must_use]
    pub fn get(&self, id: &DaemonId) -> Option<&PeerInfo> {
        self.peers.get(id).map(|record| &record.info)
    }

    /// Records a beacon that has already been decoded and auth-verified,
    /// updating liveness unconditionally and returning an event exactly
    /// when the table's externally-visible knowledge changed. See the
    /// type's "Dedup rule" docs.
    pub fn observe(
        &mut self,
        from: SocketAddr,
        beacon: &Beacon,
        now: Instant,
    ) -> Option<BeaconEvent> {
        let info = PeerInfo {
            id: beacon.machine_id.clone(),
            role: beacon.role,
            listen_addrs: beacon.listen_addrs.clone(),
            protocol: beacon.protocol,
            hlc: beacon.hlc,
            source: from,
        };

        match self.peers.get_mut(&beacon.machine_id) {
            None => {
                self.peers.insert(
                    beacon.machine_id.clone(),
                    PeerRecord {
                        info: info.clone(),
                        last_seen: now,
                    },
                );
                Some(BeaconEvent::Discovered(info))
            }
            Some(record) => {
                let changed =
                    record.info.role != info.role || record.info.listen_addrs != info.listen_addrs;
                let advanced = info.hlc > record.info.hlc;
                record.last_seen = now;
                record.info.source = from;
                if advanced || changed {
                    record.info = info;
                    Some(BeaconEvent::Refreshed(record.info.clone()))
                } else {
                    None
                }
            }
        }
    }

    /// Removes every peer not heard from (dedup or not — see `observe`)
    /// within [`PeerTable::loss_timeout`] of `now`, returning a
    /// [`BeaconEvent::Lost`] for each.
    pub fn sweep(&mut self, now: Instant) -> Vec<BeaconEvent> {
        let timed_out: Vec<DaemonId> = self
            .peers
            .iter()
            .filter(|(_, record)| {
                now.checked_duration_since(record.last_seen)
                    .is_some_and(|elapsed| elapsed >= self.loss_timeout)
            })
            .map(|(id, _)| id.clone())
            .collect();

        timed_out
            .into_iter()
            .filter_map(|id| {
                self.peers
                    .remove(&id)
                    .map(|record| BeaconEvent::Lost(record.info))
            })
            .collect()
    }
}

/// Configuration for a [`BeaconWatcher`].
#[derive(Debug, Clone)]
pub struct WatcherConfig {
    /// The cluster token every inbound beacon's `auth_tag` is checked
    /// against.
    pub token: AuthToken,
    /// The nominal interval this watcher expects peers to beacon at (see
    /// [`crate::defaults::DEFAULT_BEACON_INTERVAL`]). Used only to derive
    /// [`WatcherConfig::loss_timeout`] and the sweep tick rate — it is not
    /// itself enforced per peer.
    pub expected_interval: Duration,
    /// How many `expected_interval`s of silence before a peer is declared
    /// [`BeaconEvent::Lost`] (see [`crate::defaults::DEFAULT_LOSS_MULTIPLIER`]).
    pub loss_multiplier: u32,
}

impl WatcherConfig {
    /// Builds a config with the cluster-wide default interval and loss
    /// multiplier.
    #[must_use]
    pub const fn new(token: AuthToken) -> Self {
        Self {
            token,
            expected_interval: DEFAULT_BEACON_INTERVAL,
            loss_multiplier: DEFAULT_LOSS_MULTIPLIER,
        }
    }

    /// Overrides the expected beacon interval.
    #[must_use]
    pub const fn with_expected_interval(mut self, interval: Duration) -> Self {
        self.expected_interval = interval;
        self
    }

    /// Overrides the loss multiplier.
    #[must_use]
    pub const fn with_loss_multiplier(mut self, multiplier: u32) -> Self {
        self.loss_multiplier = multiplier;
        self
    }

    /// `expected_interval * loss_multiplier` — see
    /// [`BeaconEvent::Lost`]'s docs.
    #[must_use]
    pub fn loss_timeout(&self) -> Duration {
        self.expected_interval.saturating_mul(self.loss_multiplier)
    }
}

/// Listens for beacons on a [`crate::socket::DiscoverySocket`] and emits
/// [`BeaconEvent`]s.
///
/// # Examples
///
/// ```no_run
/// use astrs_discovery::{WatcherConfig, watcher};
/// use astrs_wire::AuthToken;
///
/// # async fn example() -> Result<(), astrs_discovery::DiscoveryError> {
/// let token = AuthToken::from_bytes([0x11; 32]);
/// let (socket, _status) = astrs_discovery::socket::bind(
///     "0.0.0.0:7409".parse().unwrap(),
///     astrs_discovery::defaults::DEFAULT_MULTICAST_GROUP,
/// )
/// .await?;
/// let (watcher, mut events) = watcher::BeaconWatcher::new(socket, WatcherConfig::new(token));
/// let _handle = watcher.spawn();
///
/// while let Some(event) = events.recv().await {
///     println!("{event:?}");
/// }
/// # Ok(())
/// # }
/// #
/// # #[tokio::main]
/// # async fn main() {
/// #     example().await.unwrap();
/// # }
/// ```
pub struct BeaconWatcher<S: DiscoverySocket> {
    socket: S,
    table: PeerTable,
    config: WatcherConfig,
    events: mpsc::UnboundedSender<BeaconEvent>,
}

impl<S: DiscoverySocket> BeaconWatcher<S> {
    /// Builds a watcher over `socket`, returning it paired with the
    /// receiving half of its event channel.
    pub fn new(socket: S, config: WatcherConfig) -> (Self, mpsc::UnboundedReceiver<BeaconEvent>) {
        let (events, receiver) = mpsc::unbounded_channel();
        let table = PeerTable::new(config.loss_timeout());
        (
            Self {
                socket,
                table,
                config,
                events,
            },
            receiver,
        )
    }

    /// The local address this watcher's socket is bound to.
    ///
    /// # Errors
    ///
    /// Whatever the underlying socket's `local_addr` reports.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// A read-only view of the watcher's current peer knowledge, mainly for
    /// tests; production callers should consume the event channel returned
    /// by [`BeaconWatcher::new`] instead of polling this.
    #[must_use]
    pub fn peers(&self) -> &PeerTable {
        &self.table
    }

    /// Runs the receive-and-sweep loop until the socket reports a receive
    /// error (including, for this crate's own `#[cfg(test)]`-only
    /// `test_support` fake socket, the
    /// injector side being dropped).
    ///
    /// There is currently no separate graceful-shutdown signal: a caller
    /// that needs to stop a running watcher drops or aborts the
    /// [`tokio::task::JoinHandle`] returned by [`BeaconWatcher::spawn`].
    pub async fn run(mut self) {
        let mut buf = vec![0u8; RECV_BUFFER_LEN];
        let sweep_period = self.config.expected_interval.max(Duration::from_millis(1));
        let mut sweep = tokio::time::interval(sweep_period);
        sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                received = self.socket.recv_from(&mut buf) => {
                    match received {
                        Ok((len, from)) => self.handle_datagram(&buf[..len], len, from),
                        Err(err) => {
                            tracing::warn!(error = %err, "discovery socket recv failed; stopping watcher loop");
                            return;
                        }
                    }
                }
                _ = sweep.tick() => {
                    for event in self.table.sweep(Instant::now()) {
                        // The receiver dropping is not this loop's problem
                        // to react to; it just means nobody is listening
                        // anymore, and the loop keeps sweeping in case
                        // that changes (the socket may still be shared).
                        let _ = self.events.send(event);
                    }
                }
            }
        }
    }

    /// Spawns [`BeaconWatcher::run`] on the current `tokio` runtime.
    pub fn spawn(self) -> tokio::task::JoinHandle<()>
    where
        S: 'static,
    {
        tokio::spawn(self.run())
    }

    fn handle_datagram(&mut self, bytes: &[u8], len: usize, from: SocketAddr) {
        if len >= RECV_BUFFER_LEN {
            tracing::debug!(
                %from,
                len,
                max = RECV_BUFFER_LEN,
                "oversized beacon datagram dropped"
            );
            return;
        }

        match Beacon::decode_and_verify(bytes, &self.config.token) {
            Ok(beacon) => {
                if !beacon.speaks_our_protocol() {
                    tracing::debug!(
                        %from,
                        peer_protocol = beacon.protocol,
                        "beacon from a peer speaking a different protocol version"
                    );
                }
                if let Some(event) = self.table.observe(from, &beacon, Instant::now()) {
                    let _ = self.events.send(event);
                }
            }
            // Expected, routine outcomes for a shared multicast group
            // (foreign protocols, other clusters, transient corruption):
            // logged quietly, never propagated as a loop-ending failure.
            Err(err) => tracing::debug!(%from, error = %err, "beacon dropped"),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use astrs_time::{Clock, ManualClock};
    use std::time::Duration;

    fn token() -> AuthToken {
        AuthToken::from_bytes([42; 32])
    }

    fn peer(label: &str) -> DaemonId {
        format!("{label}-00000000-0000-0000-0000-0000000000ab")
            .parse()
            .unwrap()
    }

    fn beacon_from(id: &DaemonId, hlc: HlcTimestamp, addrs: Vec<SocketAddr>) -> Beacon {
        Beacon::signed(BeaconRole::Daemon, id.clone(), addrs, hlc, &token()).unwrap()
    }

    fn source() -> SocketAddr {
        "10.0.0.5:7408".parse().unwrap()
    }

    // --- PeerTable: pure state machine -----------------------------------

    #[test]
    fn first_sighting_is_discovered() {
        let mut table = PeerTable::new(Duration::from_secs(3));
        let now = Instant::now();
        let id = peer("a");
        let beacon = beacon_from(&id, HlcTimestamp::new(1, 0), vec![]);

        let event = table.observe(source(), &beacon, now).unwrap();
        assert_eq!(
            event,
            BeaconEvent::Discovered(table.get(&id).unwrap().clone())
        );
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn advancing_hlc_from_a_known_peer_refreshes() {
        let mut table = PeerTable::new(Duration::from_secs(3));
        let now = Instant::now();
        let id = peer("a");

        table.observe(
            source(),
            &beacon_from(&id, HlcTimestamp::new(1, 0), vec![]),
            now,
        );
        let event = table
            .observe(
                source(),
                &beacon_from(&id, HlcTimestamp::new(2, 0), vec![]),
                now,
            )
            .unwrap();
        assert!(matches!(event, BeaconEvent::Refreshed(_)));
        assert_eq!(table.get(&id).unwrap().hlc, HlcTimestamp::new(2, 0));
    }

    #[test]
    fn duplicate_hlc_from_a_known_peer_is_silently_deduplicated() {
        let mut table = PeerTable::new(Duration::from_secs(3));
        let now = Instant::now();
        let id = peer("a");
        let beacon = beacon_from(&id, HlcTimestamp::new(5, 0), vec![]);

        assert!(table.observe(source(), &beacon, now).is_some());
        // The exact same beacon delivered a second time (multicast
        // fan-out's classic duplicate delivery).
        let event = table.observe(source(), &beacon, now + Duration::from_millis(1));
        assert_eq!(event, None);
    }

    #[test]
    fn stale_hlc_from_a_known_peer_is_ignored_but_still_counts_as_liveness() {
        let mut table = PeerTable::new(Duration::from_secs(3));
        let now = Instant::now();
        let id = peer("a");

        table.observe(
            source(),
            &beacon_from(&id, HlcTimestamp::new(10, 0), vec![]),
            now,
        );
        // A reordered, older-HLC packet arrives after a newer one already
        // landed: must not regress the recorded HLC, and must not emit an
        // event, but it did just prove the peer is alive.
        let later = now + Duration::from_millis(500);
        let event = table.observe(
            source(),
            &beacon_from(&id, HlcTimestamp::new(3, 0), vec![]),
            later,
        );
        assert_eq!(event, None);
        assert_eq!(table.get(&id).unwrap().hlc, HlcTimestamp::new(10, 0));

        // No `Lost` even close to the (unadvanced) timeout, because the
        // stale packet still refreshed `last_seen`.
        assert!(table.sweep(later + Duration::from_millis(2_900)).is_empty());
    }

    #[test]
    fn changed_addrs_at_the_same_hlc_still_refreshes() {
        // The documented escape hatch: a caller that violates the
        // fresh-DaemonId-per-restart contract still converges instead of
        // being wedged behind a frozen HLC.
        let mut table = PeerTable::new(Duration::from_secs(3));
        let now = Instant::now();
        let id = peer("a");
        let hlc = HlcTimestamp::new(7, 0);

        table.observe(
            source(),
            &beacon_from(&id, hlc, vec!["10.0.0.5:7408".parse().unwrap()]),
            now,
        );
        let event = table
            .observe(
                source(),
                &beacon_from(&id, hlc, vec!["10.0.0.6:7408".parse().unwrap()]),
                now,
            )
            .unwrap();
        assert!(matches!(event, BeaconEvent::Refreshed(_)));
        assert_eq!(
            table.get(&id).unwrap().listen_addrs,
            vec!["10.0.0.6:7408".parse().unwrap()]
        );
    }

    #[test]
    fn source_address_always_refreshes_even_when_deduplicated() {
        let mut table = PeerTable::new(Duration::from_secs(3));
        let now = Instant::now();
        let id = peer("a");
        let beacon = beacon_from(&id, HlcTimestamp::new(1, 0), vec![]);
        let first_hop: SocketAddr = "10.0.0.5:7408".parse().unwrap();
        let second_hop: SocketAddr = "10.0.0.6:7408".parse().unwrap();

        table.observe(first_hop, &beacon, now);
        assert_eq!(table.get(&id).unwrap().source, first_hop);

        // Same (deduplicated) beacon content, but observed from a
        // different physical source this time.
        table.observe(second_hop, &beacon, now + Duration::from_millis(1));
        assert_eq!(table.get(&id).unwrap().source, second_hop);
    }

    #[test]
    fn distinct_peers_are_tracked_independently() {
        let mut table = PeerTable::new(Duration::from_secs(3));
        let now = Instant::now();
        table.observe(
            source(),
            &beacon_from(&peer("a"), HlcTimestamp::new(1, 0), vec![]),
            now,
        );
        table.observe(
            source(),
            &beacon_from(&peer("b"), HlcTimestamp::new(1, 0), vec![]),
            now,
        );
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn loss_timeout_uses_a_manual_clock_for_deterministic_elapsed_time() {
        // Demonstrates the intended integration with `astrs_time`'s
        // controllable clock: `Instant`s handed to `PeerTable` come from
        // `ManualClock::now_instant()`, advanced under full test control,
        // rather than real wall-clock sleeps.
        let clock = ManualClock::new(0);
        let loss_timeout = Duration::from_secs(1) * DEFAULT_LOSS_MULTIPLIER;
        let mut table = PeerTable::new(loss_timeout);
        let id = peer("a");

        table.observe(
            source(),
            &beacon_from(&id, HlcTimestamp::new(1, 0), vec![]),
            clock.now_instant(),
        );

        // A jittered re-announcement lands well inside the window.
        clock.advance(Duration::from_millis(1_100));
        table.observe(
            source(),
            &beacon_from(&id, HlcTimestamp::new(2, 0), vec![]),
            clock.now_instant(),
        );
        assert!(table.sweep(clock.now_instant()).is_empty());

        // Now silence for the full loss timeout since that last beacon.
        clock.advance(loss_timeout);
        let lost = table.sweep(clock.now_instant());
        assert_eq!(lost.len(), 1);
        assert!(matches!(&lost[0], BeaconEvent::Lost(info) if info.id == id));
        assert!(table.is_empty());
    }

    #[test]
    fn sweep_only_removes_peers_past_the_timeout() {
        let loss_timeout = Duration::from_secs(3);
        let mut table = PeerTable::new(loss_timeout);
        let base = Instant::now();
        let stale = peer("stale");
        let fresh = peer("fresh");

        table.observe(
            source(),
            &beacon_from(&stale, HlcTimestamp::new(1, 0), vec![]),
            base,
        );
        table.observe(
            source(),
            &beacon_from(&fresh, HlcTimestamp::new(1, 0), vec![]),
            base + Duration::from_secs(2),
        );

        // At `base + 3s`: `stale` (silent since `base`) has hit the
        // timeout exactly; `fresh` (silent only since `base + 2s`) has not.
        let lost = table.sweep(base + Duration::from_secs(3));
        assert_eq!(lost.len(), 1);
        assert!(matches!(&lost[0], BeaconEvent::Lost(info) if info.id == stale));
        assert_eq!(table.len(), 1);
        assert!(table.get(&fresh).is_some());
    }

    #[test]
    fn beacon_event_info_accessor_covers_every_variant() {
        let info = PeerInfo {
            id: peer("a"),
            role: BeaconRole::Daemon,
            listen_addrs: vec![],
            protocol: 1,
            hlc: HlcTimestamp::EPOCH,
            source: source(),
        };
        assert_eq!(BeaconEvent::Discovered(info.clone()).info(), &info);
        assert_eq!(BeaconEvent::Refreshed(info.clone()).info(), &info);
        assert_eq!(BeaconEvent::Lost(info.clone()).info(), &info);
    }

    // --- BeaconWatcher: the async loop over an injected fake socket -------

    fn watcher_config() -> WatcherConfig {
        WatcherConfig::new(token()).with_expected_interval(Duration::from_millis(30))
    }

    #[tokio::test]
    async fn a_good_beacon_produces_a_discovered_event() {
        let (socket, handle) =
            crate::test_support::channel_socket("127.0.0.1:9000".parse().unwrap());
        let (watcher, mut events) = BeaconWatcher::new(socket, watcher_config());
        let join = tokio::spawn(watcher.run());

        let id = peer("a");
        let bytes = beacon_from(&id, HlcTimestamp::new(1, 0), vec![])
            .to_bytes()
            .unwrap();
        handle.inject(bytes, source());

        let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(event, BeaconEvent::Discovered(info) if info.id == id));

        drop(handle);
        let _ = join.await;
    }

    #[tokio::test]
    async fn rejection_matrix_entries_are_dropped_and_the_loop_survives() {
        let (socket, handle) =
            crate::test_support::channel_socket("127.0.0.1:9001".parse().unwrap());
        let (watcher, mut events) = BeaconWatcher::new(socket, watcher_config());
        let join = tokio::spawn(watcher.run());

        // (a) garbage bytes.
        handle.inject(vec![0xFFu8; 3], source());
        // (b) well-formed, wrong token.
        let wrong_token_bytes = Beacon::signed(
            BeaconRole::Daemon,
            peer("wrong-cluster"),
            vec![],
            HlcTimestamp::new(1, 0),
            &AuthToken::from_bytes([99; 32]),
        )
        .unwrap()
        .to_bytes()
        .unwrap();
        handle.inject(wrong_token_bytes, source());
        // (c) oversized datagram.
        handle.inject(vec![0u8; RECV_BUFFER_LEN], source());

        // None of the above produced an event; the next good beacon still
        // does, proving the loop is still alive and correctly wired.
        let id = peer("good");
        let good_bytes = beacon_from(&id, HlcTimestamp::new(1, 0), vec![])
            .to_bytes()
            .unwrap();
        handle.inject(good_bytes, source());

        let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(event, BeaconEvent::Discovered(info) if info.id == id));

        // And still exactly one event total: the three bad datagrams truly
        // produced nothing.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.recv())
                .await
                .is_err(),
            "no further events should be queued"
        );

        drop(handle);
        let _ = join.await;
    }

    #[tokio::test]
    async fn watcher_reports_the_sockets_local_addr() {
        let addr: SocketAddr = "127.0.0.1:9002".parse().unwrap();
        let (socket, _handle) = crate::test_support::channel_socket(addr);
        let (watcher, _events) = BeaconWatcher::new(socket, watcher_config());
        assert_eq!(watcher.local_addr().unwrap(), addr);
    }

    #[tokio::test]
    async fn run_exits_when_the_socket_closes() {
        let (socket, handle) =
            crate::test_support::channel_socket("127.0.0.1:9003".parse().unwrap());
        let (watcher, _events) = BeaconWatcher::new(socket, watcher_config());
        let join = tokio::spawn(watcher.run());

        drop(handle);
        tokio::time::timeout(Duration::from_secs(1), join)
            .await
            .expect("watcher should exit promptly once its socket closes")
            .unwrap();
    }
}
