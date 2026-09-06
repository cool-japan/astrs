//! The standard UDP port mapping and the default discovery multicast group.
//!
//! RTPS turns a domain id and a participant id into four port numbers with a
//! single arithmetic rule (OMG DDSI-RTPS 2.3 §9.6.1.1). Two participants that
//! agree on the domain therefore find each other's metatraffic ports without
//! exchanging anything first, which is what makes SPDP bootstrappable at all.
//!
//! ```text
//! metatraffic multicast  =  PB + DG*domainId + d0
//! metatraffic unicast    =  PB + DG*domainId + d1 + PG*participantId
//! user      multicast    =  PB + DG*domainId + d2
//! user      unicast      =  PB + DG*domainId + d3 + PG*participantId
//!
//! PB = 7400   DG = 250   PG = 2   d0 = 0   d1 = 10   d2 = 1   d3 = 11
//! ```
//!
//! Every function here returns `Option<u16>`: the arithmetic overflows a UDP
//! port above domain 232, and a silently wrapped port is a participant that
//! joins the wrong domain.
//!
//! ```
//! use astrs_rtps::structure::port;
//!
//! // Domain 0, participant 0 — the ROS 2 default.
//! assert_eq!(port::metatraffic_multicast(0), Some(7400));
//! assert_eq!(port::metatraffic_unicast(0, 0), Some(7410));
//! assert_eq!(port::user_multicast(0), Some(7401));
//! assert_eq!(port::user_unicast(0, 0), Some(7411));
//!
//! // Domain 1 shifts every port by DG = 250.
//! assert_eq!(port::metatraffic_multicast(1), Some(7650));
//! ```
//!
//! # Scope note
//!
//! The blueprint files "standard port mapping" under discovery (§10.2), but
//! the mapping is pure arithmetic over two integers with no state and no
//! socket, so it lives here with the other value types. The behavior half
//! consumes it; nothing in this module opens anything.

use std::net::Ipv4Addr;

use crate::structure::locator::Locator;

/// `PB` — the port base every RTPS port is offset from.
pub const PORT_BASE: u32 = 7400;

/// `DG` — the ports one domain claims.
pub const DOMAIN_ID_GAIN: u32 = 250;

/// `PG` — the ports one participant claims within a domain.
pub const PARTICIPANT_ID_GAIN: u32 = 2;

/// `d0` — offset of the metatraffic (discovery) multicast port.
pub const OFFSET_METATRAFFIC_MULTICAST: u32 = 0;

/// `d1` — offset of the metatraffic (discovery) unicast port.
pub const OFFSET_METATRAFFIC_UNICAST: u32 = 10;

/// `d2` — offset of the user-traffic multicast port.
pub const OFFSET_USER_MULTICAST: u32 = 1;

/// `d3` — offset of the user-traffic unicast port.
pub const OFFSET_USER_UNICAST: u32 = 11;

/// The default SPDP discovery multicast group, `239.255.0.1` (§9.6.1.4.1).
pub const DEFAULT_MULTICAST_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 0, 1);

/// The largest domain id whose four ports all fit sixteen bits.
///
/// `PB + DG*232 + d3 = 7400 + 58000 + 11 = 65411`, and one more domain would
/// pass 65535. This is the same ceiling ROS 2 documents for `ROS_DOMAIN_ID`.
pub const MAX_DOMAIN_ID: u32 = 232;

/// Fold a computed port into `u16`, or `None` when it does not fit.
const fn narrow(port: u32) -> Option<u16> {
    if port <= u16::MAX as u32 {
        Some(port as u16)
    } else {
        None
    }
}

/// `PB + DG*domain_id + offset + PG*participant_id`, saturating.
///
/// Saturating rather than wrapping: an absurd domain id must produce `None`
/// from [`narrow`], never a plausible-looking port in someone else's domain.
const fn mapped(domain_id: u32, offset: u32, participant_id: u32) -> u32 {
    PORT_BASE
        .saturating_add(DOMAIN_ID_GAIN.saturating_mul(domain_id))
        .saturating_add(offset)
        .saturating_add(PARTICIPANT_ID_GAIN.saturating_mul(participant_id))
}

/// The multicast port participants of `domain_id` listen on for SPDP.
///
/// `PB + DG*domainId + d0`.
#[must_use]
pub const fn metatraffic_multicast(domain_id: u32) -> Option<u16> {
    narrow(mapped(domain_id, OFFSET_METATRAFFIC_MULTICAST, 0))
}

/// The unicast port participant `participant_id` of `domain_id` receives
/// metatraffic on.
///
/// `PB + DG*domainId + d1 + PG*participantId`.
#[must_use]
pub const fn metatraffic_unicast(domain_id: u32, participant_id: u32) -> Option<u16> {
    narrow(mapped(
        domain_id,
        OFFSET_METATRAFFIC_UNICAST,
        participant_id,
    ))
}

/// The multicast port user traffic of `domain_id` uses by default.
///
/// `PB + DG*domainId + d2`.
#[must_use]
pub const fn user_multicast(domain_id: u32) -> Option<u16> {
    narrow(mapped(domain_id, OFFSET_USER_MULTICAST, 0))
}

/// The unicast port participant `participant_id` of `domain_id` receives user
/// traffic on.
///
/// `PB + DG*domainId + d3 + PG*participantId`.
#[must_use]
pub const fn user_unicast(domain_id: u32, participant_id: u32) -> Option<u16> {
    narrow(mapped(domain_id, OFFSET_USER_UNICAST, participant_id))
}

/// True when every port of `domain_id` fits sixteen bits.
#[must_use]
pub const fn is_usable_domain(domain_id: u32) -> bool {
    domain_id <= MAX_DOMAIN_ID
}

/// The locator an SPDP announcement is multicast to on `domain_id`.
///
/// # Examples
///
/// ```
/// use astrs_rtps::structure::port;
///
/// let locator = port::default_multicast_locator(0).expect("domain 0 fits");
/// assert_eq!(locator.to_string(), "239.255.0.1:7400");
/// assert!(locator.is_multicast());
/// ```
#[must_use]
pub const fn default_multicast_locator(domain_id: u32) -> Option<Locator> {
    match metatraffic_multicast(domain_id) {
        Some(port) => Some(Locator::udpv4(DEFAULT_MULTICAST_GROUP, port)),
        None => None,
    }
}

/// The metatraffic unicast locator of a participant at `address`.
#[must_use]
pub const fn metatraffic_unicast_locator(
    address: Ipv4Addr,
    domain_id: u32,
    participant_id: u32,
) -> Option<Locator> {
    match metatraffic_unicast(domain_id, participant_id) {
        Some(port) => Some(Locator::udpv4(address, port)),
        None => None,
    }
}

/// The user-traffic unicast locator of a participant at `address`.
#[must_use]
pub const fn user_unicast_locator(
    address: Ipv4Addr,
    domain_id: u32,
    participant_id: u32,
) -> Option<Locator> {
    match user_unicast(domain_id, participant_id) {
        Some(port) => Some(Locator::udpv4(address, port)),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn domain_zero_is_the_ros2_default_set() {
        assert_eq!(metatraffic_multicast(0), Some(7400));
        assert_eq!(user_multicast(0), Some(7401));
        assert_eq!(metatraffic_unicast(0, 0), Some(7410));
        assert_eq!(user_unicast(0, 0), Some(7411));
    }

    #[test]
    fn each_participant_claims_two_ports() {
        assert_eq!(metatraffic_unicast(0, 1), Some(7412));
        assert_eq!(user_unicast(0, 1), Some(7413));
        assert_eq!(metatraffic_unicast(0, 2), Some(7414));
        assert_eq!(user_unicast(0, 2), Some(7415));
        // Participant 1's metatraffic port is exactly PG above participant 0's.
        let first = metatraffic_unicast(0, 0).expect("fits");
        let second = metatraffic_unicast(0, 1).expect("fits");
        assert_eq!(u32::from(second - first), PARTICIPANT_ID_GAIN);
    }

    #[test]
    fn each_domain_claims_two_hundred_and_fifty_ports() {
        for domain in 0..8_u32 {
            let expected = PORT_BASE + DOMAIN_ID_GAIN * domain;
            assert_eq!(metatraffic_multicast(domain), Some(expected as u16));
            assert_eq!(user_multicast(domain), Some((expected + 1) as u16));
            assert_eq!(metatraffic_unicast(domain, 0), Some((expected + 10) as u16));
            assert_eq!(user_unicast(domain, 0), Some((expected + 11) as u16));
        }
    }

    #[test]
    fn the_domain_ceiling_is_where_the_ports_stop_fitting() {
        assert!(is_usable_domain(MAX_DOMAIN_ID));
        assert!(!is_usable_domain(MAX_DOMAIN_ID + 1));
        assert_eq!(user_unicast(MAX_DOMAIN_ID, 0), Some(65_411));
        assert_eq!(metatraffic_multicast(MAX_DOMAIN_ID + 1), None);
        assert_eq!(user_unicast(MAX_DOMAIN_ID + 1, 0), None);
        assert_eq!(metatraffic_unicast(MAX_DOMAIN_ID + 1, 0), None);
        assert_eq!(user_multicast(MAX_DOMAIN_ID + 1), None);
        // A participant id can also push the port past the ceiling.
        assert_eq!(user_unicast(MAX_DOMAIN_ID, 100), None);
    }

    #[test]
    fn the_discovery_group_is_the_one_the_specification_names() {
        assert_eq!(DEFAULT_MULTICAST_GROUP, Ipv4Addr::new(239, 255, 0, 1));
        let locator = default_multicast_locator(2).expect("domain 2 fits");
        assert_eq!(locator.to_string(), "239.255.0.1:7900");
        assert!(locator.is_multicast());
        assert_eq!(default_multicast_locator(1_000), None);
    }

    #[test]
    fn participant_locators_pair_an_address_with_the_mapped_port() {
        let address = Ipv4Addr::new(10, 1, 2, 3);
        assert_eq!(
            metatraffic_unicast_locator(address, 0, 1)
                .expect("fits")
                .to_string(),
            "10.1.2.3:7412"
        );
        assert_eq!(
            user_unicast_locator(address, 0, 1)
                .expect("fits")
                .to_string(),
            "10.1.2.3:7413"
        );
        assert_eq!(metatraffic_unicast_locator(address, 9_999, 0), None);
        assert_eq!(user_unicast_locator(address, 9_999, 0), None);
    }
}
