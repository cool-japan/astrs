//! [`discover_coordinator`] — the rendezvous API a daemon or the CLI calls
//! to find the cluster coordinator (blueprint §6.4: "Rendezvous API used by
//! daemon/CLI in W3: `discover_coordinator(timeout, static_hint)` -> ordered
//! candidate list (static config wins over multicast; document
//! precedence)").
//!
//! # Precedence and latency contract
//!
//! - If `static_hint` names at least one coordinator candidate
//!   ([`crate::peer_book::PeerBook::coordinator_candidates`] is
//!   non-empty), this function returns them **immediately**, in their
//!   configured order, without opening a socket or waiting out `timeout`
//!   at all. A correctly configured cluster — the common case in
//!   production — pays no discovery latency whatsoever.
//! - Only when the static hint is absent or empty does this function fall
//!   back to binding a discovery socket and collecting coordinator beacons
//!   (multicast and any unicast aimed directly at it) for up to `timeout`,
//!   returning candidates in the order their beacons were first observed,
//!   deduplicated by [`astrs_wire::DaemonId`]. That fallback socket binds
//!   the multicast endpoint resolved by
//!   [`crate::defaults::multicast_endpoint_from_env`] — honoring
//!   [`crate::defaults::ENV_MULTICAST_GROUP`]/
//!   [`crate::defaults::ENV_MULTICAST_PORT`] overrides, not just the
//!   compiled-in defaults.
//!
//! Static and beacon-sourced candidates are never combined in one call:
//! precedence is total, not a tie-break within a merged list. A caller
//! that wants both — trust the static hint but also cross-check it against
//! live beacons — calls this function once with `static_hint` and, if it
//! also wants live confirmation, separately runs a
//! [`crate::watcher::BeaconWatcher`].

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use astrs_wire::{AuthToken, DaemonId};

use crate::beacon::{Beacon, BeaconRole};
use crate::defaults::{self, RECV_BUFFER_LEN};
use crate::error::DiscoveryResult;
use crate::peer_book::PeerBook;
use crate::socket::{self, DiscoverySocket};

/// Where a [`CoordinatorCandidate`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CandidateSource {
    /// Named directly by a [`PeerBook`] — always ranked ahead of any
    /// `Beacon`-sourced candidate (see the module docs' precedence
    /// contract).
    Static,
    /// Observed via a verified coordinator beacon during the collection
    /// phase.
    Beacon,
}

/// One candidate address for the cluster coordinator, ranked by
/// [`discover_coordinator`]'s precedence contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoordinatorCandidate {
    /// The address to attempt a connection to.
    pub addr: SocketAddr,
    /// Where this candidate came from.
    pub source: CandidateSource,
    /// The coordinator's identity, when known. Always `Some` for
    /// [`CandidateSource::Beacon`]; always `None` for
    /// [`CandidateSource::Static`], since a [`PeerBook`] entry is just an
    /// address with no identity attached.
    pub daemon_id: Option<DaemonId>,
}

/// Finds candidate coordinator addresses. See the module docs for the full
/// precedence and latency contract.
///
/// # Errors
///
/// [`crate::error::DiscoveryError::EnvVar`] if `static_hint` is empty/absent
/// and [`crate::defaults::ENV_MULTICAST_GROUP`]/
/// [`crate::defaults::ENV_MULTICAST_PORT`] is set but malformed — raised
/// before any socket is touched. [`crate::error::DiscoveryError::SocketBind`]
/// if `static_hint` is empty/absent, the environment resolves cleanly, and
/// the fallback discovery socket still cannot be bound. A fallback
/// collection phase that simply times out with no beacons seen is **not**
/// an error — it returns `Ok(vec![])`.
///
/// # Examples
///
/// ```
/// use astrs_discovery::{PeerBook, rendezvous::discover_coordinator};
/// use astrs_wire::AuthToken;
/// use std::time::Duration;
///
/// # async fn example() -> Result<(), astrs_discovery::DiscoveryError> {
/// let book = PeerBook::new().with_coordinator("10.0.0.1:7407".parse().unwrap());
/// let token = AuthToken::from_bytes([0x11; 32]);
///
/// // Returns immediately: no socket opened, `timeout` is never waited out.
/// let candidates = discover_coordinator(Duration::from_secs(3), Some(&book), &token).await?;
/// assert_eq!(candidates[0].addr, "10.0.0.1:7407".parse().unwrap());
/// # Ok(())
/// # }
/// #
/// # #[tokio::main]
/// # async fn main() {
/// #     example().await.unwrap();
/// # }
/// ```
pub async fn discover_coordinator(
    timeout: Duration,
    static_hint: Option<&PeerBook>,
    token: &AuthToken,
) -> DiscoveryResult<Vec<CoordinatorCandidate>> {
    if let Some(candidates) = static_candidates(static_hint) {
        return Ok(candidates);
    }

    let (group, port) = defaults::multicast_endpoint_from_env()?;
    let bind_addr = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port);
    let (socket, _status) = socket::bind(bind_addr, group).await?;
    Ok(collect_via_beacons(&socket, timeout, token).await)
}

/// The static half of [`discover_coordinator`]'s precedence contract:
/// `Some(candidates)` (possibly checked and found non-empty) short-circuits
/// the caller before any socket is touched; `None` means "fall through to
/// the beacon collection phase".
fn static_candidates(static_hint: Option<&PeerBook>) -> Option<Vec<CoordinatorCandidate>> {
    let addrs = static_hint?.coordinator_candidates();
    if addrs.is_empty() {
        return None;
    }
    Some(
        addrs
            .iter()
            .map(|&addr| CoordinatorCandidate {
                addr,
                source: CandidateSource::Static,
                daemon_id: None,
            })
            .collect(),
    )
}

/// The beacon-collection half: generic over [`DiscoverySocket`] so it is
/// testable against an in-memory fake with no real network involved (the
/// production entry point, [`discover_coordinator`], calls this with a
/// real, bound-and-joined [`crate::socket::TokioSocket`]).
async fn collect_via_beacons<S: DiscoverySocket>(
    socket: &S,
    timeout: Duration,
    token: &AuthToken,
) -> Vec<CoordinatorCandidate> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    let mut buf = vec![0u8; RECV_BUFFER_LEN];

    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            () = &mut deadline => break,
            received = socket.recv_from(&mut buf) => {
                let Ok((len, from)) = received else { break };
                if len >= RECV_BUFFER_LEN {
                    continue;
                }
                let Ok(beacon) = Beacon::decode_and_verify(&buf[..len], token) else {
                    continue;
                };
                if beacon.role != BeaconRole::Coordinator {
                    continue;
                }
                if !seen.insert(beacon.machine_id.clone()) {
                    // Already have this coordinator's candidates from an
                    // earlier beacon within this same collection window.
                    continue;
                }
                push_candidates(&mut candidates, &beacon, from);
            }
        }
    }

    candidates
}

/// Appends one beacon's contribution to `candidates`: every announced
/// `listen_addrs` entry, or — if the beacon claims none — the address its
/// packet physically arrived from, as a last-resort candidate.
fn push_candidates(candidates: &mut Vec<CoordinatorCandidate>, beacon: &Beacon, from: SocketAddr) {
    if beacon.listen_addrs.is_empty() {
        candidates.push(CoordinatorCandidate {
            addr: from,
            source: CandidateSource::Beacon,
            daemon_id: Some(beacon.machine_id.clone()),
        });
        return;
    }
    candidates.extend(
        beacon
            .listen_addrs
            .iter()
            .map(|&addr| CoordinatorCandidate {
                addr,
                source: CandidateSource::Beacon,
                daemon_id: Some(beacon.machine_id.clone()),
            }),
    );
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use astrs_wire::MachineName;

    fn token() -> AuthToken {
        AuthToken::from_bytes([5; 32])
    }

    fn coordinator_beacon(label: &str, addrs: Vec<SocketAddr>) -> Beacon {
        let id = DaemonId::generate(Some(MachineName::new(label).unwrap()));
        Beacon::signed(
            BeaconRole::Coordinator,
            id,
            addrs,
            HlcTimestampFixture::next(),
            &token(),
        )
        .unwrap()
    }

    fn daemon_beacon(label: &str) -> Beacon {
        let id = DaemonId::generate(Some(MachineName::new(label).unwrap()));
        Beacon::signed(
            BeaconRole::Daemon,
            id,
            vec![],
            HlcTimestampFixture::next(),
            &token(),
        )
        .unwrap()
    }

    /// A trivially increasing HLC source for test fixtures, so several
    /// beacons built in one test never collide on the exact same
    /// timestamp.
    struct HlcTimestampFixture;
    impl HlcTimestampFixture {
        fn next() -> astrs_time::HlcTimestamp {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(1);
            astrs_time::HlcTimestamp::new(COUNTER.fetch_add(1, Ordering::Relaxed), 0)
        }
    }

    fn source() -> SocketAddr {
        "192.168.1.1:7409".parse().unwrap()
    }

    // --- Static precedence --------------------------------------------

    #[tokio::test]
    async fn a_non_empty_static_hint_returns_immediately_in_configured_order() {
        let book = PeerBook::new()
            .with_coordinator("10.0.0.1:7407".parse().unwrap())
            .with_coordinator("10.0.0.2:7407".parse().unwrap());

        // A huge timeout that would make the test suite hang for real if
        // the multicast fallback path were ever entered.
        let start = std::time::Instant::now();
        let candidates = discover_coordinator(Duration::from_secs(3_600), Some(&book), &token())
            .await
            .unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "a non-empty static hint must short-circuit instantly"
        );

        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].addr, "10.0.0.1:7407".parse().unwrap());
        assert_eq!(candidates[0].source, CandidateSource::Static);
        assert_eq!(candidates[0].daemon_id, None);
        assert_eq!(candidates[1].addr, "10.0.0.2:7407".parse().unwrap());
    }

    #[test]
    fn an_empty_or_absent_static_hint_falls_through() {
        assert!(static_candidates(None).is_none());
        let empty_book = PeerBook::new();
        assert!(static_candidates(Some(&empty_book)).is_none());
    }

    // --- Beacon collection (generic, no real sockets) -------------------

    #[tokio::test]
    async fn collects_coordinator_beacons_in_arrival_order() {
        let (socket, handle) =
            crate::test_support::channel_socket("127.0.0.1:9200".parse().unwrap());

        let first = coordinator_beacon("coord-a", vec!["10.0.0.1:7407".parse().unwrap()]);
        let second = coordinator_beacon("coord-b", vec!["10.0.0.2:7407".parse().unwrap()]);
        handle.inject(first.to_bytes().unwrap(), source());
        handle.inject(second.to_bytes().unwrap(), source());

        let candidates = collect_via_beacons(&socket, Duration::from_millis(100), &token()).await;
        assert_eq!(
            candidates.iter().map(|c| c.addr).collect::<Vec<_>>(),
            vec![
                "10.0.0.1:7407".parse().unwrap(),
                "10.0.0.2:7407".parse().unwrap(),
            ]
        );
        assert!(
            candidates
                .iter()
                .all(|c| c.source == CandidateSource::Beacon)
        );
        assert!(candidates.iter().all(|c| c.daemon_id.is_some()));
    }

    #[tokio::test]
    async fn ignores_daemon_beacons_and_deduplicates_repeated_coordinator_beacons() {
        let (socket, handle) =
            crate::test_support::channel_socket("127.0.0.1:9201".parse().unwrap());

        handle.inject(
            daemon_beacon("not-a-coordinator").to_bytes().unwrap(),
            source(),
        );
        let coordinator = coordinator_beacon("coord-a", vec!["10.0.0.1:7407".parse().unwrap()]);
        handle.inject(coordinator.to_bytes().unwrap(), source());
        // The identical beacon again — must not duplicate the candidate.
        handle.inject(coordinator.to_bytes().unwrap(), source());

        let candidates = collect_via_beacons(&socket, Duration::from_millis(100), &token()).await;
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].addr, "10.0.0.1:7407".parse().unwrap());
    }

    #[tokio::test]
    async fn a_coordinator_beacon_with_no_listen_addrs_falls_back_to_the_packet_source() {
        let (socket, handle) =
            crate::test_support::channel_socket("127.0.0.1:9202".parse().unwrap());
        handle.inject(
            coordinator_beacon("coord-a", vec![]).to_bytes().unwrap(),
            source(),
        );

        let candidates = collect_via_beacons(&socket, Duration::from_millis(100), &token()).await;
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].addr, source());
    }

    #[tokio::test]
    async fn malformed_and_wrong_token_beacons_do_not_stop_collection() {
        let (socket, handle) =
            crate::test_support::channel_socket("127.0.0.1:9203".parse().unwrap());

        handle.inject(vec![0xFFu8; 3], source());
        let wrong_token = Beacon::signed(
            BeaconRole::Coordinator,
            DaemonId::generate(Some(MachineName::new("foreign").unwrap())),
            vec!["10.0.0.9:7407".parse().unwrap()],
            HlcTimestampFixture::next(),
            &AuthToken::from_bytes([9; 32]),
        )
        .unwrap();
        handle.inject(wrong_token.to_bytes().unwrap(), source());
        let good = coordinator_beacon("coord-a", vec!["10.0.0.1:7407".parse().unwrap()]);
        handle.inject(good.to_bytes().unwrap(), source());

        let candidates = collect_via_beacons(&socket, Duration::from_millis(100), &token()).await;
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].addr, "10.0.0.1:7407".parse().unwrap());
    }

    #[tokio::test]
    async fn an_empty_collection_window_returns_an_empty_list_not_an_error() {
        let (socket, _handle) =
            crate::test_support::channel_socket("127.0.0.1:9204".parse().unwrap());
        let candidates = collect_via_beacons(&socket, Duration::from_millis(30), &token()).await;
        assert!(candidates.is_empty());
    }

    #[tokio::test]
    async fn collection_stops_promptly_when_the_socket_closes() {
        let (socket, handle) =
            crate::test_support::channel_socket("127.0.0.1:9205".parse().unwrap());
        drop(handle);

        let start = std::time::Instant::now();
        let candidates = collect_via_beacons(&socket, Duration::from_secs(3_600), &token()).await;
        assert!(candidates.is_empty());
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "a closed socket must not force waiting out the full timeout"
        );
    }
}
