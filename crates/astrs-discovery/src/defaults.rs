//! Well-known addresses, timings and environment variables (blueprint §6.4,
//! §24.2).
//!
//! Every constant here is a *default*: every API that consumes one also
//! accepts an explicit override, matching the rest of AstRS's
//! configuration story (blueprint §24.2 — "Default | Override" for every
//! tunable).

use std::net::Ipv4Addr;
use std::time::Duration;

use crate::error::{DiscoveryError, DiscoveryResult};

/// The multicast group AstRS beacons announce to.
///
/// `239.255.0.0/16` is IANA's "organization-local scope" block (RFC 2365),
/// the correct choice for a discovery protocol that must never leak past a
/// site's own routers. Within that block, `239.255.74.7` deliberately
/// echoes the coordinator's control-plane port (7407, itself chosen to
/// honor Atom's birthday — blueprint §4.2) in its last two octets, so a
/// packet capture showing `74.7` in the destination address is immediately
/// recognizable as "the AstRS thing" the way `7407`/`7408` already are for
/// the coordinator/daemon TCP ports.
pub const DEFAULT_MULTICAST_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 74, 7);

/// The UDP port AstRS beacons are sent to and listened on.
///
/// Continues the coordinator (7407) / daemon (7408) port sequence from
/// blueprint §4.2: 7409 is the discovery beacon's well-known rendezvous
/// port, shared by every host in the cluster regardless of role.
pub const DEFAULT_MULTICAST_PORT: u16 = 7409;

/// The nominal (pre-jitter) period between two beacons from the same
/// sender.
///
/// [`crate::jitter::jittered_interval`] draws the *actual* sleep for each
/// tick from `[DEFAULT_BEACON_INTERVAL * (1 - DEFAULT_JITTER_RATIO),
/// DEFAULT_BEACON_INTERVAL * (1 + DEFAULT_JITTER_RATIO)]`; a listener's
/// [`crate::watcher::WatcherConfig::loss_timeout`] is derived from this same
/// nominal value multiplied by [`DEFAULT_LOSS_MULTIPLIER`], not from any
/// single jittered draw.
pub const DEFAULT_BEACON_INTERVAL: Duration = Duration::from_secs(1);

/// The fractional jitter applied to every beacon send interval: ±20%.
///
/// Jitter exists to avoid a "beacon storm" — every daemon in a cluster
/// restarted by the same event (a power cycle, a coordinated deploy)
/// sending its announcement on the exact same schedule forever after.
pub const DEFAULT_JITTER_RATIO: f64 = 0.20;

/// How many nominal beacon intervals of silence from a peer before a
/// [`crate::watcher::BeaconWatcher`] declares it [`crate::watcher::BeaconEvent::Lost`].
///
/// Three intervals tolerates up to two consecutive missed beacons (dropped
/// packets, a transient scheduling stall) before acting — the same
/// heartbeat-multiplier convention used for `health_check_interval`
/// elsewhere in the blueprint (§24.2) — while the worst-case single
/// jittered gap (`DEFAULT_BEACON_INTERVAL * (1 + DEFAULT_JITTER_RATIO)` =
/// 1.2s at the defaults) stays comfortably under one third of the
/// resulting 3s timeout, so ordinary jitter alone never trips it.
pub const DEFAULT_LOSS_MULTIPLIER: u32 = 3;

/// The maximum number of `listen_addrs` a single [`crate::beacon::Beacon`]
/// may carry.
///
/// A daemon or coordinator realistically advertises a handful of reachable
/// addresses (one per NIC/hostname); a beacon claiming more is either a
/// misconfiguration or hostile, and is rejected during decode rather than
/// accepted and silently truncated later (see [`crate::beacon::Beacon`]'s
/// `Decode` impl).
pub const MAX_BEACON_LISTEN_ADDRS: usize = 8;

/// The byte length of a [`crate::beacon::Beacon`]'s `auth_tag`.
///
/// Sixteen bytes (128 bits) of a truncated HMAC-SHA-384 is far beyond
/// brute-force reach for a per-packet liveness check, while keeping the
/// wire cost of a mostly-cosmetic field small on a datagram sent every
/// second by every process in the cluster. See
/// [`crate::beacon::Beacon::signed`] for the full construction.
pub const AUTH_TAG_LEN: usize = 16;

/// The receive buffer size used by every socket that reads beacons.
///
/// A real beacon (see [`MAX_BEACON_LISTEN_ADDRS`]) never approaches this;
/// it is sized so generously above any legitimate beacon that a datagram
/// filling the buffer completely is itself grounds for suspicion —
/// [`crate::socket::DiscoverySocket`]'s callers reject rather than risk
/// decoding a silently-truncated payload (UDP's `recvfrom` truncates a
/// datagram larger than the supplied buffer without any error indication).
pub const RECV_BUFFER_LEN: usize = 2048;

/// Domain-separation prefix mixed into every beacon's `auth_tag` MAC input.
///
/// The cluster auth token is also the seed for the QUIC PSK (blueprint
/// §16), so a value derived from it must never be usable as a substitute
/// for a value derived for a different purpose. Prefixing a fixed,
/// protocol-and-version-specific label before the MAC input follows the
/// same discipline as TLS's / QUIC's own label-prefixed HKDF construction,
/// at HMAC-input cost instead of a full KDF: computing
/// `HMAC(token, "astrs-discovery/beacon/v1" || fields)` for beacons can
/// never collide with a hypothetical future `HMAC(token, "astrs-transport/"
/// || ...)` computed over the same field bytes for a different purpose.
pub const BEACON_MAC_CONTEXT: &[u8] = b"astrs-discovery/beacon/v1";

/// Environment variable naming a comma-separated list of `host:port`
/// coordinator candidates, highest-preference first (see
/// [`crate::peer_book::PeerBook::from_env`]).
pub const ENV_COORDINATOR_ADDR: &str = "ASTRS_COORDINATOR_ADDR";

/// Environment variable naming the static daemon address book: `;`-separated
/// `label=host:port,host:port` groups (see
/// [`crate::peer_book::PeerBook::from_env`]).
pub const ENV_DAEMON_ADDRS: &str = "ASTRS_DAEMON_ADDRS";

/// Environment variable overriding [`DEFAULT_MULTICAST_GROUP`].
///
/// Read by [`multicast_endpoint_from_env`]; must parse as an IPv4 address
/// inside the multicast range (`224.0.0.0/4`).
pub const ENV_MULTICAST_GROUP: &str = "ASTRS_DISCOVERY_MULTICAST_GROUP";

/// Environment variable overriding [`DEFAULT_MULTICAST_PORT`].
///
/// Read by [`multicast_endpoint_from_env`]; must parse as a `u16` other
/// than `0`.
pub const ENV_MULTICAST_PORT: &str = "ASTRS_DISCOVERY_MULTICAST_PORT";

/// Environment variable that gates the tests in this crate which require a
/// real, working multicast-capable network stack (set to `1` to enable).
///
/// Multicast is routinely unavailable in sandboxes, containers and CI
/// runners (no IGMP support, network namespaces without a multicast route,
/// virtualization layers that drop it outright). Tests guarded by this
/// variable are skipped — reported, not silently omitted — rather than
/// failing the suite on infrastructure this crate is specifically designed
/// to degrade gracefully around (see [`crate::socket::MulticastStatus`]).
pub const ENV_TEST_MULTICAST: &str = "ASTRS_TEST_MULTICAST";

/// Resolves the multicast rendezvous endpoint, honoring
/// [`ENV_MULTICAST_GROUP`] / [`ENV_MULTICAST_PORT`] overrides over
/// [`DEFAULT_MULTICAST_GROUP`] / [`DEFAULT_MULTICAST_PORT`].
///
/// The two variables are independent — either, both, or neither may be set.
/// This is the *one* place that turns them into a concrete endpoint
/// (blueprint §3.3's "one spec per concern" applies to a configuration knob
/// exactly as much as to a wire format: two independent places re-deriving
/// "the multicast endpoint" from the same two variables is the seam where
/// they drift) — but each side of the crate still has to call it:
///
/// - [`crate::rendezvous::discover_coordinator`]'s fallback
///   beacon-collection phase resolves through this function automatically,
///   on every call.
/// - [`crate::sender::SenderConfig::new`] does **not** — a
///   [`SenderConfig`](crate::sender::SenderConfig) only picks up the
///   override if the caller opts in with
///   [`with_multicast_target_from_env`](crate::sender::SenderConfig::with_multicast_target_from_env).
///
/// A process that wants its own announcements and its own fallback
/// rendezvous listening to agree — the common case for a daemon that both
/// beacons and calls `discover_coordinator` — must call both. Setting
/// `ENV_MULTICAST_PORT` and building a `SenderConfig` with plain `new`
/// leaves that sender announcing on the compiled-in default port while
/// everything reading this function directly (like the fallback rendezvous
/// above) has already moved: "override applies automatically" is true of
/// this function's callers, not of every value that happens to start from
/// [`DEFAULT_MULTICAST_GROUP`]/[`DEFAULT_MULTICAST_PORT`].
///
/// # Errors
///
/// [`DiscoveryError::EnvVar`] if a variable is set but does not parse under
/// its documented shape:
///
/// - [`ENV_MULTICAST_GROUP`] must parse as an IPv4 address inside the
///   multicast range (`224.0.0.0/4`, RFC 5771); a well-formed-but-unicast
///   address is rejected exactly like unparseable garbage, since sending to
///   or joining a unicast "group" is not multicast at all and would silently
///   defeat the entire point of the override.
/// - [`ENV_MULTICAST_PORT`] must parse as a `u16` other than `0`: port `0`
///   asks the OS to assign a fresh ephemeral port on every bind, which
///   defeats a *well-known* rendezvous port shared by every host in the
///   cluster just as thoroughly as a typo would.
///
/// # Examples
///
/// ```
/// use astrs_discovery::defaults::multicast_endpoint_from_env;
///
/// // This doctest does not mutate the environment (see this module's own
/// // env-var tests for the override cases): with neither variable set,
/// // resolution falls back to the built-in defaults and always yields a
/// // valid multicast group and a non-zero port.
/// let (group, port) = multicast_endpoint_from_env()?;
/// assert!(group.is_multicast());
/// assert_ne!(port, 0);
/// # Ok::<(), astrs_discovery::DiscoveryError>(())
/// ```
pub fn multicast_endpoint_from_env() -> DiscoveryResult<(Ipv4Addr, u16)> {
    let group = match std::env::var(ENV_MULTICAST_GROUP) {
        Ok(value) => parse_multicast_group(&value)?,
        Err(_) => DEFAULT_MULTICAST_GROUP,
    };
    let port = match std::env::var(ENV_MULTICAST_PORT) {
        Ok(value) => parse_multicast_port(&value)?,
        Err(_) => DEFAULT_MULTICAST_PORT,
    };
    Ok((group, port))
}

/// Parses and range-checks a candidate [`ENV_MULTICAST_GROUP`] value. See
/// [`multicast_endpoint_from_env`]'s docs for the exact rule.
fn parse_multicast_group(value: &str) -> DiscoveryResult<Ipv4Addr> {
    let addr: Ipv4Addr = value.parse().map_err(|err| DiscoveryError::EnvVar {
        var: ENV_MULTICAST_GROUP,
        value: value.to_owned(),
        reason: format!("{value:?} is not a valid IPv4 address: {err}"),
    })?;
    if !addr.is_multicast() {
        return Err(DiscoveryError::EnvVar {
            var: ENV_MULTICAST_GROUP,
            value: value.to_owned(),
            reason: "address is not in the IPv4 multicast range 224.0.0.0/4 (RFC 5771)".to_owned(),
        });
    }
    Ok(addr)
}

/// Parses and range-checks a candidate [`ENV_MULTICAST_PORT`] value. See
/// [`multicast_endpoint_from_env`]'s docs for the exact rule.
fn parse_multicast_port(value: &str) -> DiscoveryResult<u16> {
    let port: u16 = value.parse().map_err(|err| DiscoveryError::EnvVar {
        var: ENV_MULTICAST_PORT,
        value: value.to_owned(),
        reason: format!("{value:?} is not a valid port number: {err}"),
    })?;
    if port == 0 {
        return Err(DiscoveryError::EnvVar {
            var: ENV_MULTICAST_PORT,
            value: value.to_owned(),
            reason: "port 0 would defeat a well-known rendezvous port shared by the whole cluster"
                .to_owned(),
        });
    }
    Ok(port)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::test_support::with_env_vars;

    #[test]
    fn endpoint_from_env_defaults_when_unset() {
        with_env_vars(
            &[(ENV_MULTICAST_GROUP, None), (ENV_MULTICAST_PORT, None)],
            || {
                let (group, port) = multicast_endpoint_from_env().unwrap();
                assert_eq!(group, DEFAULT_MULTICAST_GROUP);
                assert_eq!(port, DEFAULT_MULTICAST_PORT);
            },
        );
    }

    #[test]
    fn endpoint_from_env_honors_a_group_override_alone() {
        with_env_vars(
            &[
                (ENV_MULTICAST_GROUP, Some("239.1.2.3")),
                (ENV_MULTICAST_PORT, None),
            ],
            || {
                let (group, port) = multicast_endpoint_from_env().unwrap();
                assert_eq!(group, Ipv4Addr::new(239, 1, 2, 3));
                assert_eq!(port, DEFAULT_MULTICAST_PORT);
            },
        );
    }

    #[test]
    fn endpoint_from_env_honors_a_port_override_alone() {
        with_env_vars(
            &[
                (ENV_MULTICAST_GROUP, None),
                (ENV_MULTICAST_PORT, Some("9999")),
            ],
            || {
                let (group, port) = multicast_endpoint_from_env().unwrap();
                assert_eq!(group, DEFAULT_MULTICAST_GROUP);
                assert_eq!(port, 9999);
            },
        );
    }

    #[test]
    fn endpoint_from_env_honors_both_overrides_together() {
        with_env_vars(
            &[
                (ENV_MULTICAST_GROUP, Some("239.9.9.9")),
                (ENV_MULTICAST_PORT, Some("12345")),
            ],
            || {
                let (group, port) = multicast_endpoint_from_env().unwrap();
                assert_eq!(group, Ipv4Addr::new(239, 9, 9, 9));
                assert_eq!(port, 12345);
            },
        );
    }

    #[test]
    fn endpoint_from_env_rejects_an_unparseable_group() {
        with_env_vars(
            &[
                (ENV_MULTICAST_GROUP, Some("not-an-ip")),
                (ENV_MULTICAST_PORT, None),
            ],
            || {
                let err = multicast_endpoint_from_env().unwrap_err();
                assert!(
                    matches!(err, DiscoveryError::EnvVar { var, .. } if var == ENV_MULTICAST_GROUP)
                );
            },
        );
    }

    #[test]
    fn endpoint_from_env_rejects_a_unicast_group() {
        // Well-formed IPv4, but outside 224.0.0.0/4 — not a multicast
        // address at all, so joining or sending to it would silently do
        // nothing useful.
        with_env_vars(
            &[
                (ENV_MULTICAST_GROUP, Some("10.0.0.1")),
                (ENV_MULTICAST_PORT, None),
            ],
            || {
                let err = multicast_endpoint_from_env().unwrap_err();
                match err {
                    DiscoveryError::EnvVar { var, reason, .. } => {
                        assert_eq!(var, ENV_MULTICAST_GROUP);
                        assert!(reason.contains("multicast"));
                    }
                    other => panic!("unexpected {other:?}"),
                }
            },
        );
    }

    #[test]
    fn endpoint_from_env_rejects_an_unparseable_port() {
        with_env_vars(
            &[
                (ENV_MULTICAST_GROUP, None),
                (ENV_MULTICAST_PORT, Some("not-a-port")),
            ],
            || {
                let err = multicast_endpoint_from_env().unwrap_err();
                assert!(
                    matches!(err, DiscoveryError::EnvVar { var, .. } if var == ENV_MULTICAST_PORT)
                );
            },
        );
    }

    #[test]
    fn endpoint_from_env_rejects_port_zero() {
        with_env_vars(
            &[(ENV_MULTICAST_GROUP, None), (ENV_MULTICAST_PORT, Some("0"))],
            || {
                let err = multicast_endpoint_from_env().unwrap_err();
                match err {
                    DiscoveryError::EnvVar { var, reason, .. } => {
                        assert_eq!(var, ENV_MULTICAST_PORT);
                        assert!(reason.contains('0'));
                    }
                    other => panic!("unexpected {other:?}"),
                }
            },
        );
    }

    #[test]
    fn multicast_group_is_organization_local_scope() {
        // RFC 2365: 239.255.0.0/16 is administratively scoped to a site.
        assert_eq!(DEFAULT_MULTICAST_GROUP.octets()[0], 239);
        assert_eq!(DEFAULT_MULTICAST_GROUP.octets()[1], 255);
        assert!(DEFAULT_MULTICAST_GROUP.is_multicast());
    }

    #[test]
    fn loss_timeout_margin_holds_against_worst_case_jitter() {
        let worst_case_gap = DEFAULT_BEACON_INTERVAL.mul_f64(1.0 + DEFAULT_JITTER_RATIO);
        let loss_timeout = DEFAULT_BEACON_INTERVAL * DEFAULT_LOSS_MULTIPLIER;
        assert!(
            worst_case_gap < loss_timeout,
            "a single ordinary jittered gap ({worst_case_gap:?}) must never \
             alone reach the loss timeout ({loss_timeout:?})"
        );
        // The margin comfortably tolerates one full missed beacon on top of
        // the worst-case jittered gap (two total intervals of silence)
        // without crossing the timeout.
        assert!(worst_case_gap + DEFAULT_BEACON_INTERVAL < loss_timeout);
    }

    #[test]
    fn ports_are_distinct_from_the_control_plane() {
        assert_eq!(DEFAULT_MULTICAST_PORT, 7409);
        assert_ne!(DEFAULT_MULTICAST_PORT, 7407);
        assert_ne!(DEFAULT_MULTICAST_PORT, 7408);
    }

    #[test]
    fn env_var_names_share_the_astrs_prefix() {
        for name in [
            ENV_COORDINATOR_ADDR,
            ENV_DAEMON_ADDRS,
            ENV_MULTICAST_GROUP,
            ENV_MULTICAST_PORT,
            ENV_TEST_MULTICAST,
        ] {
            assert!(name.starts_with("ASTRS_"), "{name} must be ASTRS_-prefixed");
        }
    }
}
