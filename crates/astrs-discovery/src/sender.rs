//! [`BeaconSender`] — the periodic multicast-and-unicast beacon announcer
//! (blueprint §6.4: "periodic UDP multicast announce ... carrying an
//! oxicode-encoded `Beacon` ... unicast fallback list").
//!
//! Every tick: sign a fresh [`crate::beacon::Beacon`] stamped with the
//! current HLC, send it to the configured multicast target, then send the
//! identical bytes to every address in [`SenderConfig::unicast_fallback`]
//! — belt and suspenders for networks that filter multicast between
//! subnets. A failure to reach the multicast target degrades the sender's
//! reported [`MulticastStatus`] rather than stopping the loop (the unicast
//! fallback list, and the next tick's multicast attempt, both still run).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use astrs_time::{HlcClock, SystemClock};
use astrs_wire::{AuthToken, DaemonId};

use crate::beacon::{Beacon, BeaconRole};
use crate::defaults::{
    DEFAULT_BEACON_INTERVAL, DEFAULT_JITTER_RATIO, DEFAULT_MULTICAST_GROUP, DEFAULT_MULTICAST_PORT,
    multicast_endpoint_from_env,
};
use crate::error::DiscoveryResult;
use crate::jitter::jittered_interval;
use crate::socket::{DegradedReason, DiscoverySocket, MulticastStatus};

/// Configuration for a [`BeaconSender`].
#[derive(Debug, Clone)]
pub struct SenderConfig {
    /// This sender's own role.
    pub role: BeaconRole,
    /// This sender's own per-process identity. See
    /// [`crate::beacon::Beacon`]'s "Identity and dedup" docs: mint a fresh
    /// one with [`DaemonId::generate`] per process start.
    pub machine_id: DaemonId,
    /// Addresses this sender is reachable at, announced verbatim in every
    /// beacon.
    pub listen_addrs: Vec<SocketAddr>,
    /// The cluster token every beacon is signed with.
    pub token: AuthToken,
    /// Where multicast beacons are sent
    /// (default: [`crate::defaults::DEFAULT_MULTICAST_GROUP`]:[`crate::defaults::DEFAULT_MULTICAST_PORT`]).
    pub multicast_target: SocketAddr,
    /// Additional specific addresses every beacon is also unicast to.
    pub unicast_fallback: Vec<SocketAddr>,
    /// The nominal (pre-jitter) send interval.
    pub interval: Duration,
    /// The jitter ratio applied to `interval` (see
    /// [`crate::jitter::jittered_interval`]).
    pub jitter_ratio: f64,
}

impl SenderConfig {
    /// Builds a config with the cluster-wide default multicast target,
    /// interval and jitter ratio, and an empty unicast fallback list.
    #[must_use]
    pub fn new(
        role: BeaconRole,
        machine_id: DaemonId,
        listen_addrs: Vec<SocketAddr>,
        token: AuthToken,
    ) -> Self {
        Self {
            role,
            machine_id,
            listen_addrs,
            token,
            multicast_target: SocketAddr::new(
                DEFAULT_MULTICAST_GROUP.into(),
                DEFAULT_MULTICAST_PORT,
            ),
            unicast_fallback: Vec::new(),
            interval: DEFAULT_BEACON_INTERVAL,
            jitter_ratio: DEFAULT_JITTER_RATIO,
        }
    }

    /// Overrides the unicast fallback list.
    #[must_use]
    pub fn with_unicast_fallback(mut self, addrs: Vec<SocketAddr>) -> Self {
        self.unicast_fallback = addrs;
        self
    }

    /// Overrides the multicast target address.
    #[must_use]
    pub const fn with_multicast_target(mut self, addr: SocketAddr) -> Self {
        self.multicast_target = addr;
        self
    }

    /// Overrides [`SenderConfig::multicast_target`] with whatever
    /// [`crate::defaults::multicast_endpoint_from_env`] resolves to —
    /// i.e. applies
    /// [`ENV_MULTICAST_GROUP`](crate::defaults::ENV_MULTICAST_GROUP)/
    /// [`ENV_MULTICAST_PORT`](crate::defaults::ENV_MULTICAST_PORT) on top of
    /// whatever this config's target was set to before this call. Ordinary
    /// builder semantics: call this *after*
    /// [`SenderConfig::with_multicast_target`] if the environment should be
    /// allowed to win over an explicit address, or skip it entirely for a
    /// caller that wants the environment variables to have no effect at all.
    ///
    /// [`crate::rendezvous::discover_coordinator`]'s own fallback bind
    /// resolves through the same function, so a daemon that calls both this
    /// method and `discover_coordinator` sees one consistent multicast
    /// endpoint rather than two independently-drifting ones.
    ///
    /// # Errors
    ///
    /// [`crate::error::DiscoveryError::EnvVar`] — see
    /// [`crate::defaults::multicast_endpoint_from_env`].
    pub fn with_multicast_target_from_env(mut self) -> DiscoveryResult<Self> {
        let (group, port) = multicast_endpoint_from_env()?;
        self.multicast_target = SocketAddr::new(group.into(), port);
        Ok(self)
    }

    /// Overrides the nominal send interval.
    #[must_use]
    pub const fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }
}

/// Periodically announces a [`crate::beacon::Beacon`] over multicast and a
/// configured unicast fallback list.
///
/// # Examples
///
/// ```no_run
/// use astrs_discovery::sender::{BeaconSender, SenderConfig};
/// use astrs_discovery::BeaconRole;
/// use astrs_time::HlcClock;
/// use astrs_wire::{AuthToken, DaemonId, MachineName};
/// use std::sync::Arc;
///
/// # async fn example() -> Result<(), astrs_discovery::DiscoveryError> {
/// let token = AuthToken::from_bytes([0x11; 32]);
/// let machine_id = DaemonId::generate(Some(MachineName::new("arm-01").unwrap()));
/// let config = SenderConfig::new(BeaconRole::Daemon, machine_id, vec!["10.0.0.5:7408".parse().unwrap()], token);
///
/// let (socket, _status) = astrs_discovery::socket::bind(
///     "0.0.0.0:0".parse().unwrap(),
///     astrs_discovery::defaults::DEFAULT_MULTICAST_GROUP,
/// )
/// .await?;
/// let sender = BeaconSender::new(socket, config, Arc::new(HlcClock::system()));
/// let _handle = sender.spawn();
/// # Ok(())
/// # }
/// #
/// # #[tokio::main]
/// # async fn main() {
/// #     example().await.unwrap();
/// # }
/// ```
pub struct BeaconSender<S: DiscoverySocket> {
    socket: S,
    config: SenderConfig,
    hlc: Arc<HlcClock<SystemClock>>,
    tick: AtomicU64,
    status: Mutex<MulticastStatus>,
}

impl<S: DiscoverySocket> BeaconSender<S> {
    /// Builds a sender. `hlc` is typically shared (via the `Arc`) with the
    /// rest of the owning process's event loop (blueprint §4.3).
    #[must_use]
    pub fn new(socket: S, config: SenderConfig, hlc: Arc<HlcClock<SystemClock>>) -> Self {
        Self {
            socket,
            config,
            hlc,
            tick: AtomicU64::new(0),
            // Optimistic initial state: nothing has failed yet. The first
            // tick either confirms this or corrects it.
            status: Mutex::new(MulticastStatus::Joined),
        }
    }

    /// The most recently observed multicast send status.
    #[must_use]
    pub fn multicast_status(&self) -> MulticastStatus {
        self.status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set_status(&self, status: MulticastStatus) {
        *self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = status;
    }

    /// Runs the send loop forever (until the caller aborts the
    /// [`tokio::task::JoinHandle`] returned by [`BeaconSender::spawn`], or
    /// this future is otherwise dropped).
    pub async fn run(self) {
        loop {
            let tick = self.tick.fetch_add(1, Ordering::Relaxed);
            self.send_one_beacon().await;
            let interval = jittered_interval(
                self.config.interval,
                self.config.jitter_ratio,
                &self.config.machine_id,
                tick,
            );
            tokio::time::sleep(interval).await;
        }
    }

    /// Spawns [`BeaconSender::run`] on the current `tokio` runtime.
    pub fn spawn(self) -> tokio::task::JoinHandle<()>
    where
        S: 'static,
    {
        tokio::spawn(self.run())
    }

    async fn send_one_beacon(&self) {
        let hlc = self.hlc.now();
        let beacon = match Beacon::signed(
            self.config.role,
            self.config.machine_id.clone(),
            self.config.listen_addrs.clone(),
            hlc,
            &self.config.token,
        ) {
            Ok(beacon) => beacon,
            // Unreachable in practice (see `Beacon::signed`'s docs); never
            // worth stopping the whole send loop over.
            Err(err) => {
                tracing::error!(error = %err, "failed to sign beacon; skipping this tick");
                return;
            }
        };
        let bytes = match beacon.to_bytes() {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::error!(error = %err, "failed to encode beacon; skipping this tick");
                return;
            }
        };

        match self
            .socket
            .send_to(&bytes, self.config.multicast_target)
            .await
        {
            Ok(_) => self.set_status(MulticastStatus::Joined),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    target = %self.config.multicast_target,
                    "multicast beacon send failed; unicast fallback list (if any) still runs"
                );
                self.set_status(MulticastStatus::Degraded {
                    reason: DegradedReason::SendFailed(Arc::from(err.to_string())),
                });
            }
        }

        for addr in &self.config.unicast_fallback {
            if let Err(err) = self.socket.send_to(&bytes, *addr).await {
                tracing::debug!(error = %err, %addr, "unicast fallback beacon send failed");
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::beacon::Beacon;

    fn machine_id(label: &str) -> DaemonId {
        format!("{label}-00000000-0000-0000-0000-0000000000ab")
            .parse()
            .unwrap()
    }

    fn config() -> SenderConfig {
        SenderConfig::new(
            BeaconRole::Daemon,
            machine_id("sender"),
            vec!["10.0.0.5:7408".parse().unwrap()],
            AuthToken::from_bytes([3; 32]),
        )
        .with_interval(Duration::from_millis(15))
    }

    #[test]
    fn with_multicast_target_from_env_applies_both_overrides() {
        crate::test_support::with_env_vars(
            &[
                (crate::defaults::ENV_MULTICAST_GROUP, Some("239.5.5.5")),
                (crate::defaults::ENV_MULTICAST_PORT, Some("15000")),
            ],
            || {
                let cfg = config().with_multicast_target_from_env().unwrap();
                assert_eq!(cfg.multicast_target, "239.5.5.5:15000".parse().unwrap());
            },
        );
    }

    #[test]
    fn with_multicast_target_from_env_keeps_the_default_when_unset() {
        crate::test_support::with_env_vars(
            &[
                (crate::defaults::ENV_MULTICAST_GROUP, None),
                (crate::defaults::ENV_MULTICAST_PORT, None),
            ],
            || {
                let before = config().multicast_target;
                let cfg = config().with_multicast_target_from_env().unwrap();
                assert_eq!(cfg.multicast_target, before);
            },
        );
    }

    #[test]
    fn with_multicast_target_from_env_propagates_a_malformed_override() {
        crate::test_support::with_env_vars(
            &[
                (crate::defaults::ENV_MULTICAST_GROUP, None),
                (crate::defaults::ENV_MULTICAST_PORT, Some("not-a-port")),
            ],
            || {
                let err = config().with_multicast_target_from_env().unwrap_err();
                assert!(matches!(
                    err,
                    crate::error::DiscoveryError::EnvVar { var, .. }
                        if var == crate::defaults::ENV_MULTICAST_PORT
                ));
            },
        );
    }

    #[tokio::test]
    async fn each_tick_sends_to_the_multicast_target_and_every_fallback() {
        let fallback_a: SocketAddr = "10.0.0.10:7409".parse().unwrap();
        let fallback_b: SocketAddr = "10.0.0.11:7409".parse().unwrap();
        let cfg = config().with_unicast_fallback(vec![fallback_a, fallback_b]);
        let multicast_target = cfg.multicast_target;

        let (socket, handle) =
            crate::test_support::channel_socket("127.0.0.1:9100".parse().unwrap());
        let sender = BeaconSender::new(socket, cfg, Arc::new(HlcClock::system()));
        let join = tokio::spawn(sender.run());

        let mut targets = Vec::new();
        for _ in 0..3 {
            let (target, _bytes) = tokio::time::timeout(Duration::from_secs(1), handle.next_sent())
                .await
                .unwrap()
                .unwrap();
            targets.push(target);
        }
        join.abort();

        assert_eq!(targets, vec![multicast_target, fallback_a, fallback_b]);
    }

    #[tokio::test]
    async fn sent_bytes_decode_and_verify_as_the_configured_identity() {
        let cfg = config();
        let token = cfg.token.clone();
        let expected_id = cfg.machine_id.clone();

        let (socket, handle) =
            crate::test_support::channel_socket("127.0.0.1:9101".parse().unwrap());
        let sender = BeaconSender::new(socket, cfg, Arc::new(HlcClock::system()));
        let join = tokio::spawn(sender.run());

        let (_target, bytes) = tokio::time::timeout(Duration::from_secs(1), handle.next_sent())
            .await
            .unwrap()
            .unwrap();
        join.abort();

        let beacon = Beacon::decode_and_verify(&bytes, &token).unwrap();
        assert_eq!(beacon.machine_id, expected_id);
        assert_eq!(beacon.role, BeaconRole::Daemon);
    }

    #[tokio::test]
    async fn multiple_ticks_produce_multiple_sends_over_time() {
        let cfg = config();
        let multicast_target = cfg.multicast_target;
        let (socket, handle) =
            crate::test_support::channel_socket("127.0.0.1:9102".parse().unwrap());
        let sender = BeaconSender::new(socket, cfg, Arc::new(HlcClock::system()));
        let join = tokio::spawn(sender.run());

        for _ in 0..3 {
            let (target, _bytes) = tokio::time::timeout(Duration::from_secs(1), handle.next_sent())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(target, multicast_target);
        }
        join.abort();
    }

    #[tokio::test]
    async fn status_starts_joined_and_degrades_when_sends_fail() {
        let (socket, handle) =
            crate::test_support::channel_socket("127.0.0.1:9103".parse().unwrap());
        // Dropping the handle immediately closes the channel `send_to`
        // writes into, so every subsequent send on `socket` fails —
        // simulating an unreachable multicast route with no real network
        // involved.
        drop(handle);

        let sender = BeaconSender::new(socket, config(), Arc::new(HlcClock::system()));
        assert_eq!(sender.multicast_status(), MulticastStatus::Joined);

        sender.send_one_beacon().await;
        assert!(matches!(
            sender.multicast_status(),
            MulticastStatus::Degraded {
                reason: DegradedReason::SendFailed(_)
            }
        ));
    }

    #[tokio::test]
    async fn a_beacon_send_failure_does_not_stop_the_loop() {
        // Even in the degraded state above, `run` must keep ticking (and
        // keep trying) rather than exiting — there is always a chance
        // multicast recovers, and the unicast fallback list may still be
        // reachable even when the multicast target is not.
        let (socket, handle) =
            crate::test_support::channel_socket("127.0.0.1:9104".parse().unwrap());
        drop(handle);

        let cfg = config();
        let sender = BeaconSender::new(socket, cfg, Arc::new(HlcClock::system()));
        let join = tokio::spawn(sender.run());

        // Give it a few ticks' worth of wall time; a crashed/exited task
        // would already be finished by the time we check.
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            !join.is_finished(),
            "sender loop must survive send failures"
        );
        join.abort();
    }
}
