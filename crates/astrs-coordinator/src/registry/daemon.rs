//! The in-memory daemon registry: who is connected, how to reach them, and
//! the heartbeat watchdog that notices when they stop answering (blueprint
//! §5.2, §12).
//!
//! The durable half of a daemon's identity — its [`DaemonInfo`], survivable
//! across a coordinator restart — lives in `astrs-store`
//! ([`astrs_store::record::DaemonRecord`]). This registry is the *live*
//! half: the outbound channel a session task drains to push
//! [`CoordinatorEvent`]s at a connected daemon, and the liveness bookkeeping
//! that only makes sense while a socket is actually open. A coordinator
//! restart loses this registry entirely and rebuilds it as daemons
//! reconnect and re-register — which is exactly why the durable half is
//! elsewhere.

use std::collections::HashMap;

use astrs_time::HlcTimestamp;
use astrs_wire::{CoordinatorEvent, DaemonId, MachineName, SessionId};
use tokio::sync::mpsc;

use crate::error::{CoordinatorError, Result};

/// How reachable a registered daemon currently appears.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonLiveness {
    /// Heartbeats are arriving within the expected interval.
    Connected,
    /// At least [`DaemonRegistry::missed_heartbeat_limit`] consecutive
    /// heartbeats have been missed, but the connection has not (yet)
    /// closed — the coordinator still holds its send channel open, but
    /// this daemon's own state should be treated as unconfirmed.
    Degraded,
}

/// One connected daemon, as the coordinator's live session state sees it.
pub struct DaemonHandle {
    /// The daemon's identity.
    pub id: DaemonId,
    /// The machine it registered under, if any.
    pub machine: Option<MachineName>,
    /// The address *peers* dial to reach this daemon, exactly as its
    /// [`astrs_wire::DaemonRegistration`] announced it (§4.2).
    ///
    /// Lives here rather than only in the durable [`astrs_wire::DaemonInfo`]
    /// because the one caller that needs it — building the
    /// [`astrs_wire::CoordinatorEvent::PeerRoutes`] directives for a spawn
    /// dispatch (§6.4) — runs inside a synchronous registry borrow and cannot
    /// await a store read.
    pub peer_address: String,
    /// The session this connection belongs to (for `StateCatchUp`
    /// resumption bookkeeping).
    pub session: SessionId,
    /// The channel a session task drains to push [`CoordinatorEvent`]s to
    /// this daemon. Closed (and the handle removed) when the connection
    /// ends.
    pub sender: mpsc::Sender<CoordinatorEvent>,
    /// The dataflows this daemon currently hosts at least one node of.
    pub dataflows: std::collections::BTreeSet<astrs_wire::DataflowId>,
    /// When the last sign of life (any `DaemonEvent`, not only a
    /// heartbeat) was observed.
    last_seen: HlcTimestamp,
    /// Consecutive heartbeat intervals with no sign of life.
    missed: u32,
    /// This connection's own outgoing heartbeat counter.
    heartbeat_seq: u64,
}

impl DaemonHandle {
    /// A freshly connected daemon, first seen at `now`.
    #[must_use]
    pub fn new(
        id: DaemonId,
        machine: Option<MachineName>,
        session: SessionId,
        sender: mpsc::Sender<CoordinatorEvent>,
        now: HlcTimestamp,
    ) -> Self {
        Self {
            id,
            machine,
            peer_address: String::new(),
            session,
            sender,
            dataflows: std::collections::BTreeSet::new(),
            last_seen: now,
            missed: 0,
            heartbeat_seq: 0,
        }
    }

    /// Records the address peers dial to reach this daemon (§4.2, §6.4).
    ///
    /// A builder rather than a `new` parameter so every existing construction
    /// site — including the several that have no peer address to give — keeps
    /// compiling and keeps meaning what it did.
    #[must_use]
    pub fn with_peer_address(mut self, address: impl Into<String>) -> Self {
        self.peer_address = address.into();
        self
    }

    /// Whether this daemon announced an address peers can dial.
    ///
    /// A daemon that binds no peer listener registers none, and no
    /// cross-daemon route can be pointed at it — which is a placement
    /// problem the caller must report rather than a route to open blindly.
    #[must_use]
    pub fn is_dialable(&self) -> bool {
        !self.peer_address.is_empty()
    }

    /// This daemon's current liveness classification.
    #[must_use]
    pub const fn liveness(&self, missed_heartbeat_limit: u32) -> DaemonLiveness {
        if self.missed >= missed_heartbeat_limit {
            DaemonLiveness::Degraded
        } else {
            DaemonLiveness::Connected
        }
    }

    /// How many consecutive heartbeat intervals have passed with no sign
    /// of life.
    #[must_use]
    pub const fn missed(&self) -> u32 {
        self.missed
    }

    /// When this daemon was last heard from.
    #[must_use]
    pub const fn last_seen(&self) -> HlcTimestamp {
        self.last_seen
    }

    /// Queues an event for this daemon's writer task.
    ///
    /// # Errors
    ///
    /// [`CoordinatorError::DaemonNotConnected`] if the daemon's session has
    /// already ended (its writer task dropped the receiving half).
    pub fn send(&self, event: CoordinatorEvent) -> Result<()> {
        self.sender
            .try_send(event)
            .map_err(|_| CoordinatorError::DaemonNotConnected(self.id.clone()))
    }

    /// The next heartbeat sequence number to send, advancing the counter.
    pub const fn next_heartbeat_seq(&mut self) -> u64 {
        self.heartbeat_seq += 1;
        self.heartbeat_seq
    }
}

/// The live daemon registry.
#[derive(Default)]
pub struct DaemonRegistry {
    daemons: HashMap<DaemonId, DaemonHandle>,
}

impl DaemonRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a newly connected daemon, replacing any prior handle under
    /// the same id (a reconnect after a partition — the old handle's
    /// sender is simply dropped, closing its now-stale writer task's
    /// channel).
    pub fn insert(&mut self, handle: DaemonHandle) {
        self.daemons.insert(handle.id.clone(), handle);
    }

    /// Removes a daemon (its connection ended), returning its handle if it
    /// was present.
    pub fn remove(&mut self, id: &DaemonId) -> Option<DaemonHandle> {
        self.daemons.remove(id)
    }

    /// Looks up a daemon's live handle.
    #[must_use]
    pub fn get(&self, id: &DaemonId) -> Option<&DaemonHandle> {
        self.daemons.get(id)
    }

    /// Looks up a daemon's live handle, mutably.
    pub fn get_mut(&mut self, id: &DaemonId) -> Option<&mut DaemonHandle> {
        self.daemons.get_mut(id)
    }

    /// Whether a daemon is currently connected.
    #[must_use]
    pub fn is_connected(&self, id: &DaemonId) -> bool {
        self.daemons.contains_key(id)
    }

    /// How many daemons are connected.
    #[must_use]
    pub fn len(&self) -> usize {
        self.daemons.len()
    }

    /// Whether no daemon is connected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.daemons.is_empty()
    }

    /// Every connected daemon's id, in an unspecified order.
    pub fn ids(&self) -> impl Iterator<Item = &DaemonId> {
        self.daemons.keys()
    }

    /// Every connected daemon's handle, in an unspecified order.
    pub fn handles(&self) -> impl Iterator<Item = &DaemonHandle> {
        self.daemons.values()
    }

    /// As [`DaemonRegistry::handles`], mutably — for the coordinator's own
    /// outgoing heartbeat (blueprint §4.3), which advances each handle's
    /// `heartbeat_seq` counter as it sends.
    pub fn handles_mut(&mut self) -> impl Iterator<Item = &mut DaemonHandle> {
        self.daemons.values_mut()
    }

    /// Finds the one connected daemon registered under `machine`.
    ///
    /// Returns `None` both when no daemon claims that machine name and (by
    /// construction, since a daemon's own machine name is a per-connection
    /// value it self-reports once) when more than zero claim it — the
    /// coordinator does not attempt tie-breaking between two daemons that
    /// happen to share a configured name.
    #[must_use]
    pub fn find_by_machine(&self, machine: &MachineName) -> Option<&DaemonHandle> {
        let mut found = None;
        for handle in self.daemons.values() {
            if handle.machine.as_ref() == Some(machine) {
                if found.is_some() {
                    return None;
                }
                found = Some(handle);
            }
        }
        found
    }

    /// An arbitrary, but deterministic (lowest daemon id), connected
    /// daemon — the placement fallback for a node with no `deploy:
    /// {machine}` override in a cluster of exactly one (blueprint §4.2:
    /// "default machine = coordinator-local" reduces to "the one daemon"
    /// once any daemon at all is connected).
    #[must_use]
    pub fn any(&self) -> Option<&DaemonHandle> {
        self.daemons
            .values()
            .min_by_key(|handle| handle.id.to_string())
    }

    /// Records a sign of life from `id` — any `DaemonEvent`, not only a
    /// heartbeat — resetting its missed-heartbeat counter.
    ///
    /// No-op if `id` is not connected (a late event racing a disconnect).
    pub fn record_seen(&mut self, id: &DaemonId, at: HlcTimestamp) {
        if let Some(handle) = self.daemons.get_mut(id) {
            handle.last_seen = at;
            handle.missed = 0;
        }
    }

    /// Runs one heartbeat-watchdog sweep: every daemon silent for longer
    /// than `interval` since `now` gets its missed counter incremented.
    /// Returns the ids that just crossed into
    /// [`DaemonLiveness::Degraded`] and the ids that should now be
    /// considered lost (missed at least `2 * missed_heartbeat_limit`
    /// intervals — see this module's top-level docs for why the factor of
    /// two).
    ///
    /// Lost daemons are **not** removed by this call — the caller decides
    /// what "lost" means for the dataflows a daemon hosted (typically
    /// [`DaemonRegistry::remove`] plus a cascade of `NodeExitCause::
    /// DaemonUnreachable`) and calls that itself.
    pub fn sweep_heartbeats(
        &mut self,
        now: HlcTimestamp,
        interval: std::time::Duration,
        missed_heartbeat_limit: u32,
    ) -> HeartbeatSweep {
        let mut newly_degraded = Vec::new();
        let mut lost = Vec::new();
        for handle in self.daemons.values_mut() {
            let silent_for = now
                .physical_duration_since(&handle.last_seen)
                .unwrap_or(std::time::Duration::ZERO);
            if silent_for < interval {
                continue;
            }
            let was_degraded = handle.missed >= missed_heartbeat_limit;
            handle.missed = handle.missed.saturating_add(1);
            let is_degraded = handle.missed >= missed_heartbeat_limit;
            if is_degraded && !was_degraded {
                newly_degraded.push(handle.id.clone());
            }
            if handle.missed >= missed_heartbeat_limit.saturating_mul(2) {
                lost.push(handle.id.clone());
            }
        }
        HeartbeatSweep {
            newly_degraded,
            lost,
        }
    }
}

/// The result of one [`DaemonRegistry::sweep_heartbeats`] call.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HeartbeatSweep {
    /// Daemons that just crossed into [`DaemonLiveness::Degraded`] on this
    /// sweep (not ones that were already degraded).
    pub newly_degraded: Vec<DaemonId>,
    /// Daemons that have missed enough heartbeats to be declared lost.
    pub lost: Vec<DaemonId>,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn handle(id: DaemonId, at: HlcTimestamp) -> (DaemonHandle, mpsc::Receiver<CoordinatorEvent>) {
        let (sender, receiver) = mpsc::channel(8);
        (
            DaemonHandle {
                id,
                machine: None,
                peer_address: "tcp:127.0.0.1:7409".to_owned(),
                session: SessionId::generate(),
                sender,
                dataflows: std::collections::BTreeSet::new(),
                last_seen: at,
                missed: 0,
                heartbeat_seq: 0,
            },
            receiver,
        )
    }

    #[test]
    fn insert_get_remove_round_trip() {
        let mut registry = DaemonRegistry::new();
        let id = DaemonId::generate(None);
        let (h, _rx) = handle(id.clone(), HlcTimestamp::EPOCH);
        registry.insert(h);
        assert!(registry.is_connected(&id));
        assert_eq!(registry.len(), 1);
        assert!(registry.remove(&id).is_some());
        assert!(!registry.is_connected(&id));
        assert!(registry.is_empty());
    }

    #[test]
    fn send_reaches_the_receiver_and_fails_once_it_is_dropped() {
        let id = DaemonId::generate(None);
        let (h, mut rx) = handle(id.clone(), HlcTimestamp::EPOCH);
        h.send(CoordinatorEvent::Destroy { grace: None }).unwrap();
        assert!(rx.try_recv().is_ok());

        drop(rx);
        let err = h
            .send(CoordinatorEvent::Destroy { grace: None })
            .unwrap_err();
        assert!(matches!(err, CoordinatorError::DaemonNotConnected(got) if got == id));
    }

    #[test]
    fn heartbeat_sequence_advances_monotonically() {
        let (mut h, _rx) = handle(DaemonId::generate(None), HlcTimestamp::EPOCH);
        assert_eq!(h.next_heartbeat_seq(), 1);
        assert_eq!(h.next_heartbeat_seq(), 2);
    }

    #[test]
    fn find_by_machine_matches_a_unique_registration() {
        let mut registry = DaemonRegistry::new();
        let id = DaemonId::generate(None);
        let (mut h, _rx) = handle(id.clone(), HlcTimestamp::EPOCH);
        h.machine = Some(MachineName::new("robot-1").unwrap());
        registry.insert(h);

        let found = registry.find_by_machine(&MachineName::new("robot-1").unwrap());
        assert_eq!(found.map(|h| &h.id), Some(&id));
        assert!(
            registry
                .find_by_machine(&MachineName::new("robot-2").unwrap())
                .is_none()
        );
    }

    #[test]
    fn find_by_machine_refuses_to_pick_between_two_claimants() {
        let mut registry = DaemonRegistry::new();
        for _ in 0..2 {
            let (mut h, _rx) = handle(DaemonId::generate(None), HlcTimestamp::EPOCH);
            h.machine = Some(MachineName::new("shared").unwrap());
            registry.insert(h);
        }
        assert!(
            registry
                .find_by_machine(&MachineName::new("shared").unwrap())
                .is_none()
        );
    }

    #[test]
    fn any_picks_deterministically_among_multiple_daemons() {
        let mut registry = DaemonRegistry::new();
        for _ in 0..5 {
            let (h, _rx) = handle(DaemonId::generate(None), HlcTimestamp::EPOCH);
            registry.insert(h);
        }
        let first = registry.any().map(|h| h.id.clone());
        let second = registry.any().map(|h| h.id.clone());
        assert_eq!(
            first, second,
            "the same registry state must pick the same daemon"
        );
    }

    #[test]
    fn any_is_none_for_an_empty_registry() {
        assert!(DaemonRegistry::new().any().is_none());
    }

    #[test]
    fn record_seen_resets_the_missed_counter() {
        let mut registry = DaemonRegistry::new();
        let id = DaemonId::generate(None);
        let (mut h, _rx) = handle(id.clone(), HlcTimestamp::EPOCH);
        h.missed = 2;
        registry.insert(h);
        registry.record_seen(&id, HlcTimestamp::new(100, 0));
        assert_eq!(registry.get(&id).unwrap().missed(), 0);
        assert_eq!(
            registry.get(&id).unwrap().last_seen(),
            HlcTimestamp::new(100, 0)
        );
    }

    #[test]
    fn record_seen_on_a_disconnected_daemon_is_a_silent_no_op() {
        let mut registry = DaemonRegistry::new();
        // No panic, no error return type to check — just must not blow up.
        registry.record_seen(&DaemonId::generate(None), HlcTimestamp::EPOCH);
    }

    #[test]
    fn a_sweep_within_the_interval_changes_nothing() {
        let mut registry = DaemonRegistry::new();
        let id = DaemonId::generate(None);
        let (h, _rx) = handle(id.clone(), HlcTimestamp::new(1_000_000_000, 0));
        registry.insert(h);

        let sweep = registry.sweep_heartbeats(
            HlcTimestamp::new(1_500_000_000, 0),
            std::time::Duration::from_secs(5),
            3,
        );
        assert!(sweep.newly_degraded.is_empty());
        assert!(sweep.lost.is_empty());
        assert_eq!(registry.get(&id).unwrap().missed(), 0);
    }

    #[test]
    fn repeated_silence_degrades_then_declares_lost() {
        let mut registry = DaemonRegistry::new();
        let id = DaemonId::generate(None);
        let (h, _rx) = handle(id.clone(), HlcTimestamp::new(0, 0));
        registry.insert(h);

        let interval = std::time::Duration::from_secs(5);
        // Every sweep is far enough past `last_seen` (which this test never
        // refreshes) to count as silence throughout.
        for expected_missed in 1..=6u32 {
            let now = HlcTimestamp::new(u64::from(expected_missed) * 6_000_000_000, 0);
            let sweep = registry.sweep_heartbeats(now, interval, 3);
            assert_eq!(registry.get(&id).unwrap().missed(), expected_missed);
            if expected_missed == 3 {
                assert_eq!(sweep.newly_degraded, vec![id.clone()]);
            } else {
                assert!(
                    sweep.newly_degraded.is_empty(),
                    "only the crossing sweep reports it"
                );
            }
            if expected_missed == 6 {
                assert_eq!(sweep.lost, vec![id.clone()]);
            } else {
                assert!(sweep.lost.is_empty());
            }
        }
    }

    #[test]
    fn handles_mut_reaches_every_daemon_and_advances_its_own_counter() {
        let mut registry = DaemonRegistry::new();
        for _ in 0..3 {
            let (h, _rx) = handle(DaemonId::generate(None), HlcTimestamp::EPOCH);
            registry.insert(h);
        }
        for handle in registry.handles_mut() {
            assert_eq!(handle.next_heartbeat_seq(), 1);
        }
        for handle in registry.handles() {
            assert_eq!(handle.liveness(3), DaemonLiveness::Connected);
        }
    }

    #[test]
    fn liveness_reflects_the_configured_limit() {
        let (mut h, _rx) = handle(DaemonId::generate(None), HlcTimestamp::EPOCH);
        assert_eq!(h.liveness(3), DaemonLiveness::Connected);
        h.missed = 3;
        assert_eq!(h.liveness(3), DaemonLiveness::Degraded);
    }
}
