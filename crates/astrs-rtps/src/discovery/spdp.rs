//! SPDP: how a participant announces itself and finds strangers.
//!
//! The Simple Participant Discovery Protocol is one best-effort,
//! transient-local builtin writer repeating one sample (§8.5.3). The sample
//! is [`ParticipantData`]; this module decides what goes in it, and where it
//! is sent.
//!
//! # Two rendezvous paths, and why both exist
//!
//! 1. **Multicast** — `239.255.0.1` at the §9.6.1.1 metatraffic port. The
//!    zero-configuration path, and what every ROS 2 stack uses by default.
//! 2. **Initial peers** — a configured list of unicast addresses, each sent
//!    the identical announcement.
//!
//! Blueprint §10.2 requires *both*, and the reason is not redundancy: a
//! sandboxed macOS host refuses `IP_ADD_MEMBERSHIP`, so a test that depends
//! on multicast is a test that reports "skipped" on the developer's own
//! machine. Every deterministic protocol assertion in this crate runs over
//! the initial-peers path, which needs no kernel permission at all, and the
//! multicast path is probed and its outcome asserted rather than assumed.
//!
//! # The port-0 rule
//!
//! [`SpdpConfig`] carries the domain and participant ids so the §9.6.1.1
//! ports can be *computed*, but the announcement is built from the locators
//! the caller passes in — which the participant reads back from
//! `local_addr()` after binding. A participant that binds an ephemeral port
//! and announces the computed one produces a graph that looks perfect and
//! moves no data, and it is silent: nothing logs, nothing errors, peers
//! simply send into a void. [`Spdp::new`] takes the bound locators as
//! arguments for exactly that reason — there is no path through this module
//! that can announce a port nothing is listening on.

use std::time::{Duration as StdDuration, Instant};

use crate::behavior::error::{BehaviorError, BehaviorResult};

use crate::discovery::builtin::BuiltinEndpointSet;
use crate::discovery::compat::RosCompat;
use crate::discovery::db::DiscoveryDb;
use crate::discovery::participant_data::ParticipantData;
use crate::structure::{Duration, Guid, GuidPrefix, Locator, port};

/// How often a participant re-announces itself by default.
///
/// Three seconds is what ROS 2's `rmw` implementations use, and it sits well
/// inside the hundred-second default lease.
pub const DEFAULT_ANNOUNCE_PERIOD: StdDuration = StdDuration::from_secs(3);

/// The lease a participant asks peers to honour by default.
pub const DEFAULT_LEASE: Duration = Duration::DEFAULT_PARTICIPANT_LEASE;

/// The `PID_USER_DATA` ROS 2 puts in every participant announcement.
pub const ROS2_DEFAULT_ENCLAVE: &[u8] = b"enclave=/;";

/// How a participant's SPDP half is configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpdpConfig {
    /// The DDS domain.
    pub domain_id: u32,
    /// The participant index within the domain, for the §9.6.1.1 port map.
    pub participant_id: u32,
    /// This participant's GUID prefix.
    pub guid_prefix: GuidPrefix,
    /// How long peers should wait before declaring this participant gone.
    pub lease_duration: Duration,
    /// How often to re-announce.
    pub announce_period: StdDuration,
    /// Unicast addresses to announce to directly.
    pub initial_peers: Vec<Locator>,
    /// Whether to announce to the multicast group as well.
    pub multicast_enabled: bool,
    /// Which ROS 2 distribution's conventions to speak.
    pub compat: RosCompat,
    /// A human-readable name for the participant.
    pub entity_name: Option<String>,
    /// Opaque `PID_USER_DATA`.
    pub user_data: Vec<u8>,
    /// Which builtin endpoints this participant runs.
    pub builtin_endpoints: BuiltinEndpointSet,
}

impl SpdpConfig {
    /// A configuration for `domain_id` with no initial peers and multicast
    /// on.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::DomainIdOutOfRange`] when the domain is above what
    /// the §9.6.1.1 mapping can express.
    pub fn new(
        domain_id: u32,
        participant_id: u32,
        guid_prefix: GuidPrefix,
    ) -> BehaviorResult<Self> {
        if !port::is_usable_domain(domain_id) {
            return Err(BehaviorError::DomainIdOutOfRange {
                domain_id,
                maximum: port::MAX_DOMAIN_ID,
            });
        }
        if port::metatraffic_unicast(domain_id, participant_id).is_none() {
            return Err(BehaviorError::ParticipantIdOutOfRange {
                participant_id,
                domain_id,
            });
        }
        Ok(Self {
            domain_id,
            participant_id,
            guid_prefix,
            lease_duration: DEFAULT_LEASE,
            announce_period: DEFAULT_ANNOUNCE_PERIOD,
            initial_peers: Vec::new(),
            multicast_enabled: true,
            compat: RosCompat::default(),
            entity_name: None,
            user_data: ROS2_DEFAULT_ENCLAVE.to_vec(),
            builtin_endpoints: BuiltinEndpointSet::ASTRS,
        })
    }

    /// Add a unicast peer to announce to.
    #[must_use]
    pub fn with_initial_peer(mut self, peer: Locator) -> Self {
        if !self.initial_peers.contains(&peer) {
            self.initial_peers.push(peer);
        }
        self
    }

    /// Replace the whole initial-peer list.
    #[must_use]
    pub fn with_initial_peers(mut self, peers: Vec<Locator>) -> Self {
        self.initial_peers = peers;
        self
    }

    /// Turn multicast announcement on or off.
    #[must_use]
    pub const fn with_multicast(mut self, enabled: bool) -> Self {
        self.multicast_enabled = enabled;
        self
    }

    /// Replace the announce cadence.
    #[must_use]
    pub const fn with_announce_period(mut self, period: StdDuration) -> Self {
        self.announce_period = period;
        self
    }

    /// Replace the lease this participant asks for.
    #[must_use]
    pub const fn with_lease(mut self, lease: Duration) -> Self {
        self.lease_duration = lease;
        self
    }

    /// Select the ROS 2 distribution.
    #[must_use]
    pub const fn with_compat(mut self, compat: RosCompat) -> Self {
        self.compat = compat;
        self
    }

    /// Name the participant.
    #[must_use]
    pub fn with_entity_name(mut self, name: impl Into<String>) -> Self {
        self.entity_name = Some(name.into());
        self
    }

    /// This participant's GUID.
    #[must_use]
    pub const fn guid(&self) -> Guid {
        self.guid_prefix
            .with_entity(crate::structure::ENTITYID_PARTICIPANT)
    }

    /// The §9.6.1.1 metatraffic multicast port for this domain.
    #[must_use]
    pub const fn metatraffic_multicast_port(&self) -> Option<u16> {
        port::metatraffic_multicast(self.domain_id)
    }

    /// The §9.6.1.1 metatraffic unicast port for this participant.
    ///
    /// Correct for a real-world deployment, and **not** what the
    /// announcement carries when the socket was bound to port 0. See the
    /// [module documentation](self).
    #[must_use]
    pub const fn metatraffic_unicast_port(&self) -> Option<u16> {
        port::metatraffic_unicast(self.domain_id, self.participant_id)
    }

    /// The §9.6.1.1 user unicast port for this participant.
    #[must_use]
    pub const fn user_unicast_port(&self) -> Option<u16> {
        port::user_unicast(self.domain_id, self.participant_id)
    }

    /// The SPDP multicast locator for this domain.
    #[must_use]
    pub const fn multicast_locator(&self) -> Option<Locator> {
        port::default_multicast_locator(self.domain_id)
    }

    /// True when the participant can *initiate* discovery — reach a peer it
    /// has never heard from.
    ///
    /// False makes the participant **passive**: it still announces itself,
    /// but only to peers that have announced to it first. That is a legitimate
    /// and useful configuration, not an error. A participant whose
    /// metatraffic locator has been handed to someone else — by a
    /// configuration file, by a test fixture, by a coordinator — needs no
    /// initial peer of its own, because the first announcement it *receives*
    /// tells it where to answer.
    #[must_use]
    pub fn has_discovery_path(&self) -> bool {
        self.multicast_enabled || !self.initial_peers.is_empty()
    }
}

/// The SPDP half of a participant: the sample it announces and the cadence
/// that repeats it.
#[derive(Debug, Clone)]
pub struct Spdp {
    config: SpdpConfig,
    local: ParticipantData,
    last_announced_at: Option<Instant>,
    announcements: u64,
}

impl Spdp {
    /// Build the announcement from the configuration and the locators the
    /// sockets are *actually* bound to.
    ///
    /// `metatraffic_unicast` and `default_unicast` must come from
    /// `local_addr()`, never from the port map. See the [module
    /// documentation](self).
    ///
    /// A participant with neither multicast nor initial peers is *passive*
    /// rather than broken: see
    /// [`SpdpConfig::has_discovery_path`]. [`Spdp::is_passive`] reports it.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::Cdr`] can only arise from a caller-supplied user data
    /// or entity name too long to encode.
    pub fn new(
        config: SpdpConfig,
        metatraffic_unicast: Locator,
        default_unicast: Locator,
    ) -> BehaviorResult<Self> {
        let mut local = ParticipantData::new(config.guid())
            .with_domain(config.domain_id)
            .with_lease(config.lease_duration)
            .with_metatraffic_unicast(metatraffic_unicast)
            .with_default_unicast(default_unicast);
        local.available_builtin_endpoints = config.builtin_endpoints;
        local.user_data = config.user_data.clone();
        local.entity_name = config.entity_name.clone();
        if config.multicast_enabled
            && let Some(group) = config.multicast_locator()
        {
            local = local
                .with_metatraffic_multicast(group)
                .with_default_multicast(user_multicast_locator(config.domain_id));
        }
        Ok(Self {
            config,
            local,
            last_announced_at: None,
            announcements: 0,
        })
    }

    /// The configuration.
    #[must_use]
    pub const fn config(&self) -> &SpdpConfig {
        &self.config
    }

    /// The sample this participant announces.
    #[must_use]
    pub const fn local(&self) -> &ParticipantData {
        &self.local
    }

    /// This participant's GUID.
    #[must_use]
    pub const fn guid(&self) -> Guid {
        self.config.guid()
    }

    /// How many announcements have gone out.
    #[must_use]
    pub const fn announcements(&self) -> u64 {
        self.announcements
    }

    /// True when this participant can only be found, never find.
    ///
    /// A passive participant announces to every peer it already knows and to
    /// nobody else. It becomes non-passive the moment one announcement
    /// arrives.
    #[must_use]
    pub fn is_passive(&self) -> bool {
        !self.config.has_discovery_path()
    }

    /// True when the cadence says it is time to announce again.
    #[must_use]
    pub fn should_announce(&self, now: Instant) -> bool {
        match self.last_announced_at {
            None => true,
            Some(last) => now.saturating_duration_since(last) >= self.config.announce_period,
        }
    }

    /// How long until the next announcement is due.
    #[must_use]
    pub fn until_next_announcement(&self, now: Instant) -> StdDuration {
        match self.last_announced_at {
            None => StdDuration::ZERO,
            Some(last) => self
                .config
                .announce_period
                .saturating_sub(now.saturating_duration_since(last)),
        }
    }

    /// Note that an announcement has just gone out.
    pub fn mark_announced(&mut self, now: Instant) {
        self.last_announced_at = Some(now);
        self.announcements = self.announcements.saturating_add(1);
    }

    /// Where the next announcement should go.
    ///
    /// The multicast group when it is enabled, every initial peer, and the
    /// metatraffic unicast locator of every participant already known — the
    /// last of which is what makes discovery converge quickly on a host where
    /// multicast is refused: once two participants have met by any route,
    /// each re-announcement reaches the other directly.
    ///
    /// Duplicates are removed, so a peer that is both an initial peer and a
    /// known participant is sent one datagram, not two.
    #[must_use]
    pub fn announce_targets(&self, db: &DiscoveryDb) -> Vec<Locator> {
        let mut targets: Vec<Locator> = Vec::new();
        let push = |locator: Locator, targets: &mut Vec<Locator>| {
            if locator.socket_addr().is_ok() && !targets.contains(&locator) {
                targets.push(locator);
            }
        };

        if self.config.multicast_enabled
            && let Some(group) = self.config.multicast_locator()
        {
            push(group, &mut targets);
        }
        for peer in &self.config.initial_peers {
            push(*peer, &mut targets);
        }
        for locator in db.all_metatraffic_locators() {
            push(locator, &mut targets);
        }
        targets
    }

    /// Update the announcement's `PID_PARTICIPANT_MANUAL_LIVELINESS_COUNT`.
    ///
    /// §8.4.13.1: the count increments each time the application manually
    /// asserts liveliness, and a peer reading a larger count knows the
    /// participant is alive without a WLP sample having to arrive.
    pub fn bump_manual_liveliness(&mut self) -> i32 {
        self.local.manual_liveliness_count = self.local.manual_liveliness_count.saturating_add(1);
        self.local.manual_liveliness_count
    }

    /// Whether a peer's announcement is one this participant should act on.
    ///
    /// Two reasons to ignore one: it is this participant's own announcement
    /// looped back by multicast, or it is on a different domain. The domain
    /// check is skipped when the peer does not announce a domain id, because
    /// `PID_DOMAIN_ID` is a DDS-Security addition that older stacks omit and
    /// dropping those would be worse than trusting the port they arrived on.
    #[must_use]
    pub fn accepts(&self, announcement: &ParticipantData) -> bool {
        if announcement.guid.prefix == self.config.guid_prefix {
            return false;
        }
        match announcement.domain_id {
            None => true,
            Some(domain) => domain == self.config.domain_id,
        }
    }

    /// Replace the announced locators — a socket was rebound.
    pub fn set_locators(&mut self, metatraffic_unicast: Locator, default_unicast: Locator) {
        self.local.metatraffic_unicast = vec![metatraffic_unicast];
        self.local.default_unicast = vec![default_unicast];
    }
}

/// The §9.6.1.1 user-traffic multicast locator for a domain.
fn user_multicast_locator(domain_id: u32) -> Locator {
    match port::user_multicast(domain_id) {
        Some(number) => Locator::udpv4(port::DEFAULT_MULTICAST_GROUP, number),
        None => Locator::INVALID,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::structure::{ENTITYID_PARTICIPANT, VendorId};
    use std::net::Ipv4Addr;

    fn prefix(seed: u8) -> GuidPrefix {
        GuidPrefix::vendor_scoped(VendorId::ASTRS, [seed; 10])
    }

    fn bound(port: u16) -> Locator {
        Locator::udpv4(Ipv4Addr::LOCALHOST, port)
    }

    fn config() -> SpdpConfig {
        SpdpConfig::new(0, 0, prefix(1)).expect("domain 0 is usable")
    }

    fn spdp(config: SpdpConfig) -> Spdp {
        Spdp::new(config, bound(45_101), bound(45_102)).expect("the announcement must build")
    }

    #[test]
    fn an_absurd_domain_is_refused_at_configuration_time() {
        let error = SpdpConfig::new(9_999, 0, prefix(1)).expect_err("must refuse");
        assert!(matches!(error, BehaviorError::DomainIdOutOfRange { .. }));
    }

    #[test]
    fn an_absurd_participant_id_is_refused_at_configuration_time() {
        let error = SpdpConfig::new(0, 100_000, prefix(1)).expect_err("must refuse");
        assert!(matches!(
            error,
            BehaviorError::ParticipantIdOutOfRange { .. }
        ));
    }

    #[test]
    fn a_participant_with_no_peers_is_passive_not_broken() {
        let quiet = config().with_multicast(false);
        assert!(!quiet.has_discovery_path());
        let spdp = Spdp::new(quiet, bound(1), bound(2)).expect("passive is legal");
        assert!(spdp.is_passive());
        assert!(
            spdp.announce_targets(&DiscoveryDb::new()).is_empty(),
            "it has nobody to announce to yet"
        );
    }

    #[test]
    fn a_passive_participant_answers_whoever_announces_to_it() {
        let spdp = spdp(config().with_multicast(false));
        let mut db = DiscoveryDb::new();
        db.observe_participant(
            ParticipantData::new(Guid::new(prefix(2), ENTITYID_PARTICIPANT))
                .with_metatraffic_unicast(bound(45_201)),
            Instant::now(),
        );
        assert_eq!(spdp.announce_targets(&db), vec![bound(45_201)]);
    }

    #[test]
    fn initial_peers_alone_are_a_discovery_path() {
        let unicast_only = config()
            .with_multicast(false)
            .with_initial_peer(bound(7_410));
        assert!(unicast_only.has_discovery_path());
        assert!(Spdp::new(unicast_only, bound(1), bound(2)).is_ok());
    }

    #[test]
    fn the_announcement_carries_the_bound_ports_not_the_computed_ones() {
        let spdp = spdp(config());
        assert_eq!(
            spdp.local().metatraffic_unicast,
            vec![bound(45_101)],
            "the announcement must name the socket that is actually listening"
        );
        assert_eq!(spdp.local().default_unicast, vec![bound(45_102)]);
        assert_eq!(
            spdp.config().metatraffic_unicast_port(),
            Some(7_410),
            "the computed port is still available for a real deployment"
        );
        assert_ne!(
            spdp.local().metatraffic_unicast[0].udp_port(),
            spdp.config().metatraffic_unicast_port(),
        );
    }

    #[test]
    fn multicast_locators_are_announced_only_when_multicast_is_on() {
        let with = spdp(config());
        assert_eq!(
            with.local().metatraffic_multicast,
            vec![Locator::udpv4(Ipv4Addr::new(239, 255, 0, 1), 7_400)]
        );
        assert_eq!(with.local().default_multicast.len(), 1);

        let without = spdp(config().with_multicast(false).with_initial_peer(bound(1)));
        assert!(without.local().metatraffic_multicast.is_empty());
        assert!(without.local().default_multicast.is_empty());
    }

    #[test]
    fn the_announcement_carries_the_astrs_vendor_id_and_endpoint_set() {
        let spdp = spdp(config());
        assert_eq!(spdp.local().vendor_id, VendorId::ASTRS);
        assert_eq!(
            spdp.local().available_builtin_endpoints,
            BuiltinEndpointSet::ASTRS
        );
        assert_eq!(spdp.local().user_data, ROS2_DEFAULT_ENCLAVE);
        assert_eq!(spdp.guid(), Guid::new(prefix(1), ENTITYID_PARTICIPANT));
    }

    #[test]
    fn the_first_announcement_is_due_immediately() {
        let mut spdp = spdp(config());
        let now = Instant::now();
        assert!(spdp.should_announce(now));
        assert_eq!(spdp.until_next_announcement(now), StdDuration::ZERO);
        assert_eq!(spdp.announcements(), 0);

        spdp.mark_announced(now);
        assert!(!spdp.should_announce(now));
        assert_eq!(spdp.announcements(), 1);
        assert_eq!(spdp.until_next_announcement(now), DEFAULT_ANNOUNCE_PERIOD);
        assert!(spdp.should_announce(now + DEFAULT_ANNOUNCE_PERIOD));
    }

    #[test]
    fn the_cadence_is_configurable() {
        let mut spdp = spdp(config().with_announce_period(StdDuration::from_millis(20)));
        let now = Instant::now();
        spdp.mark_announced(now);
        assert!(!spdp.should_announce(now + StdDuration::from_millis(10)));
        assert!(spdp.should_announce(now + StdDuration::from_millis(20)));
    }

    #[test]
    fn targets_are_the_group_the_peers_and_everyone_already_known() {
        let spdp = spdp(config().with_initial_peer(bound(7_410)));
        let mut db = DiscoveryDb::new();
        db.observe_participant(
            ParticipantData::new(Guid::new(prefix(2), ENTITYID_PARTICIPANT))
                .with_metatraffic_unicast(bound(45_201)),
            Instant::now(),
        );

        let targets = spdp.announce_targets(&db);
        assert_eq!(targets.len(), 3);
        assert!(targets[0].is_multicast(), "the group goes first");
        assert!(targets.contains(&bound(7_410)));
        assert!(targets.contains(&bound(45_201)));
    }

    #[test]
    fn a_peer_that_is_also_an_initial_peer_is_targeted_once() {
        let spdp = spdp(
            config()
                .with_multicast(false)
                .with_initial_peer(bound(45_201)),
        );
        let mut db = DiscoveryDb::new();
        db.observe_participant(
            ParticipantData::new(Guid::new(prefix(2), ENTITYID_PARTICIPANT))
                .with_metatraffic_unicast(bound(45_201)),
            Instant::now(),
        );
        assert_eq!(spdp.announce_targets(&db), vec![bound(45_201)]);
    }

    #[test]
    fn an_unusable_target_is_dropped() {
        let spdp = spdp(
            config()
                .with_multicast(false)
                .with_initial_peers(vec![Locator::INVALID, bound(7_410)]),
        );
        assert_eq!(
            spdp.announce_targets(&DiscoveryDb::new()),
            vec![bound(7_410)]
        );
    }

    #[test]
    fn a_participant_ignores_its_own_announcement() {
        let spdp = spdp(config());
        assert!(
            !spdp.accepts(spdp.local()),
            "multicast loops the announcement back; it must not self-discover"
        );
    }

    #[test]
    fn a_peer_on_another_domain_is_ignored() {
        let spdp = spdp(config());
        let stranger =
            ParticipantData::new(Guid::new(prefix(2), ENTITYID_PARTICIPANT)).with_domain(7);
        assert!(!spdp.accepts(&stranger));
        assert!(spdp.accepts(&stranger.clone().with_domain(0)));
    }

    #[test]
    fn a_peer_that_announces_no_domain_is_trusted() {
        let spdp = spdp(config());
        let old_stack = ParticipantData::new(Guid::new(prefix(2), ENTITYID_PARTICIPANT));
        assert_eq!(old_stack.domain_id, None);
        assert!(
            spdp.accepts(&old_stack),
            "PID_DOMAIN_ID is a later addition; the port it arrived on already said the domain"
        );
    }

    #[test]
    fn the_manual_liveliness_count_increments() {
        let mut spdp = spdp(config());
        assert_eq!(spdp.local().manual_liveliness_count, 0);
        assert_eq!(spdp.bump_manual_liveliness(), 1);
        assert_eq!(spdp.bump_manual_liveliness(), 2);
        assert_eq!(spdp.local().manual_liveliness_count, 2);
    }

    #[test]
    fn locators_can_be_replaced_after_a_rebind() {
        let mut spdp = spdp(config());
        spdp.set_locators(bound(1), bound(2));
        assert_eq!(spdp.local().metatraffic_unicast, vec![bound(1)]);
        assert_eq!(spdp.local().default_unicast, vec![bound(2)]);
    }

    #[test]
    fn the_announcement_round_trips_through_its_payload() {
        let spdp = spdp(config().with_entity_name("talker"));
        let payload = spdp.local().to_payload().expect("encode");
        let decoded = ParticipantData::from_payload(&payload).expect("decode");
        assert_eq!(&decoded, spdp.local());
        assert_eq!(decoded.entity_name.as_deref(), Some("talker"));
    }

    #[test]
    fn the_compat_switch_is_carried_in_the_configuration() {
        let humble = spdp(config().with_compat(RosCompat::Humble));
        assert_eq!(humble.config().compat, RosCompat::Humble);
        assert_eq!(humble.config().compat.gid_len(), 24);

        let jazzy = spdp(config().with_compat(RosCompat::Jazzy));
        assert_eq!(jazzy.config().compat.gid_len(), 16);
    }

    #[test]
    fn the_domain_ports_are_the_ones_the_mapping_gives() {
        let config = SpdpConfig::new(2, 3, prefix(1)).unwrap();
        assert_eq!(config.metatraffic_multicast_port(), Some(7_900));
        assert_eq!(config.metatraffic_unicast_port(), Some(7_916));
        assert_eq!(config.user_unicast_port(), Some(7_917));
        assert_eq!(
            config.multicast_locator(),
            Some(Locator::udpv4(Ipv4Addr::new(239, 255, 0, 1), 7_900))
        );
    }
}
