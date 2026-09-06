//! `SPDPdiscoveredParticipantData`: what a participant says about itself.
//!
//! One sample, written by every participant to `ENTITYID_SPDP_BUILTIN_
//! PARTICIPANT_WRITER` every few seconds, carrying everything a stranger
//! needs to start talking: who it is, what it speaks, where to send, which
//! builtin endpoints it runs, and how long to wait before declaring it gone.
//! OMG DDSI-RTPS 2.3 §8.5.3.2, Table 9.14.
//!
//! # Encoding
//!
//! `PL_CDR_LE` — a four-octet encapsulation header, then `PID`-tagged
//! parameters, then `PID_SENTINEL`. Every parameter is padded to a four-octet
//! multiple, which `astrs-cdr`'s
//! [`Parameter`](astrs_cdr::Parameter) handles, so nothing in this module
//! counts octets by hand.
//!
//! # Robustness
//!
//! Decoding is deliberately lopsided. Exactly one parameter is required —
//! `PID_PARTICIPANT_GUID`, without which the sample names nobody — and
//! everything else falls back to the specification's default. Unknown
//! parameters are ignored, repeated locator parameters accumulate, and a
//! parameter whose value is the wrong length is reported rather than
//! silently half-read. That is what §8.5.3.2's forward-compatibility rule
//! asks for, and it is the difference between "a future ROS release joined
//! the graph" and "a future ROS release made every participant drop its
//! datagrams".
//!
//! # Locators and the port-0 rule
//!
//! The locators in this sample are the ones a peer will actually send to, so
//! they must name the port the socket is *bound* to, not the port §9.6.1.1
//! computes. A participant that binds an ephemeral port and announces the
//! computed one produces a graph that looks perfect and moves no data. The
//! participant builds this structure from `local_addr()` for exactly that
//! reason.

use core::fmt;

use astrs_cdr::{Encoding, ParameterId, ParameterList, pid};

use crate::behavior::error::{BehaviorError, BehaviorResult};
use crate::discovery::builtin::{BuiltinEndpointQos, BuiltinEndpointSet};
use crate::discovery::plist::{decode_bounded_string, decode_locators, decode_octets, decode_one};
use crate::messages::SerializedPayload;
use crate::structure::{Duration, Guid, Locator, ProtocolVersion, VendorId};

/// The `context` every error out of this module carries.
const CONTEXT: &str = "SPDP participant data";

/// Longest `PID_ENTITY_NAME` this decoder will accept.
pub const MAX_ENTITY_NAME_LEN: usize = 256;

/// Longest `PID_DOMAIN_TAG` this decoder will accept.
pub const MAX_DOMAIN_TAG_LEN: usize = 256;

/// Longest `PID_USER_DATA` this decoder will accept.
///
/// ROS 2 puts `enclave=/;` here, which is a few dozen octets. A megabyte of
/// "user data" in a discovery announcement is an attack, not a use case.
pub const MAX_USER_DATA_LEN: usize = 4_096;

/// The default participant lease, per §8.5.3.3: 100 seconds.
pub const DEFAULT_LEASE: Duration = Duration::DEFAULT_PARTICIPANT_LEASE;

/// Everything one participant announces about itself over SPDP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParticipantData {
    /// The participant's GUID; its entity id is always
    /// `ENTITYID_PARTICIPANT`. The one required field.
    pub guid: Guid,
    /// The RTPS version the participant speaks.
    pub protocol_version: ProtocolVersion,
    /// Who wrote the stack. AstRS announces `VendorId::ASTRS`.
    pub vendor_id: VendorId,
    /// The DDS domain, when the participant announces one.
    ///
    /// `PID_DOMAIN_ID` is not in the RTPS 2.3 table — it arrived with the
    /// DDS-Security specification and every modern stack sends it — so it is
    /// optional here rather than defaulted.
    pub domain_id: Option<u32>,
    /// The domain tag, which partitions one domain id into several logical
    /// domains. Empty means the default tag.
    pub domain_tag: Option<String>,
    /// Where to send discovery traffic, unicast.
    pub metatraffic_unicast: Vec<Locator>,
    /// Where to send discovery traffic, multicast.
    pub metatraffic_multicast: Vec<Locator>,
    /// Where to send user traffic when an endpoint announces no locator of
    /// its own, unicast.
    pub default_unicast: Vec<Locator>,
    /// The same, multicast.
    pub default_multicast: Vec<Locator>,
    /// Which builtin discovery endpoints the participant runs.
    pub available_builtin_endpoints: BuiltinEndpointSet,
    /// Per-endpoint QoS overrides for those builtin endpoints.
    pub builtin_endpoint_qos: BuiltinEndpointQos,
    /// How long a peer should wait, after the last announcement, before
    /// declaring this participant gone.
    pub lease_duration: Duration,
    /// Incremented every time the application manually asserts liveliness.
    pub manual_liveliness_count: i32,
    /// True when the participant's readers want inline QoS on every sample.
    pub expects_inline_qos: bool,
    /// Opaque application data. ROS 2 carries `enclave=…;` here.
    pub user_data: Vec<u8>,
    /// A human-readable name, when the participant sets one.
    pub entity_name: Option<String>,
}

impl ParticipantData {
    /// A minimal announcement: just an identity, everything else defaulted.
    #[must_use]
    pub fn new(guid: Guid) -> Self {
        Self {
            guid,
            protocol_version: ProtocolVersion::CURRENT,
            vendor_id: VendorId::ASTRS,
            domain_id: None,
            domain_tag: None,
            metatraffic_unicast: Vec::new(),
            metatraffic_multicast: Vec::new(),
            default_unicast: Vec::new(),
            default_multicast: Vec::new(),
            available_builtin_endpoints: BuiltinEndpointSet::ASTRS,
            builtin_endpoint_qos: BuiltinEndpointQos::NONE,
            lease_duration: DEFAULT_LEASE,
            manual_liveliness_count: 0,
            expects_inline_qos: false,
            user_data: Vec::new(),
            entity_name: None,
        }
    }

    /// Set the domain id this participant belongs to.
    #[must_use]
    pub fn with_domain(mut self, domain_id: u32) -> Self {
        self.domain_id = Some(domain_id);
        self
    }

    /// Set the lease duration peers should honour.
    #[must_use]
    pub const fn with_lease(mut self, lease_duration: Duration) -> Self {
        self.lease_duration = lease_duration;
        self
    }

    /// Add a metatraffic unicast locator.
    #[must_use]
    pub fn with_metatraffic_unicast(mut self, locator: Locator) -> Self {
        self.metatraffic_unicast.push(locator);
        self
    }

    /// Add a metatraffic multicast locator.
    #[must_use]
    pub fn with_metatraffic_multicast(mut self, locator: Locator) -> Self {
        self.metatraffic_multicast.push(locator);
        self
    }

    /// Add a default (user-traffic) unicast locator.
    #[must_use]
    pub fn with_default_unicast(mut self, locator: Locator) -> Self {
        self.default_unicast.push(locator);
        self
    }

    /// Add a default (user-traffic) multicast locator.
    #[must_use]
    pub fn with_default_multicast(mut self, locator: Locator) -> Self {
        self.default_multicast.push(locator);
        self
    }

    /// Set the human-readable participant name.
    #[must_use]
    pub fn with_entity_name(mut self, name: impl Into<String>) -> Self {
        self.entity_name = Some(name.into());
        self
    }

    /// Set the opaque user data.
    #[must_use]
    pub fn with_user_data(mut self, user_data: impl Into<Vec<u8>>) -> Self {
        self.user_data = user_data.into();
        self
    }

    /// The participant's GUID prefix.
    #[must_use]
    pub const fn guid_prefix(&self) -> crate::structure::GuidPrefix {
        self.guid.prefix
    }

    /// The locators discovery traffic should be sent to, unicast first.
    ///
    /// A peer sends its SPDP reply and every SEDP sample here. Unicast comes
    /// first because it is the deterministic path — on a host where the
    /// multicast join was refused it is the *only* path.
    #[must_use]
    pub fn metatraffic_locators(&self) -> Vec<Locator> {
        let mut locators = self.metatraffic_unicast.clone();
        locators.extend(self.metatraffic_multicast.iter().copied());
        locators
    }

    /// The locators user traffic should default to.
    #[must_use]
    pub fn default_locators(&self) -> Vec<Locator> {
        let mut locators = self.default_unicast.clone();
        locators.extend(self.default_multicast.iter().copied());
        locators
    }

    /// True when this participant can be reached at all.
    #[must_use]
    pub fn is_reachable(&self) -> bool {
        self.metatraffic_locators()
            .iter()
            .any(|locator| locator.socket_addr().is_ok())
    }

    /// True when `other` names the same participant.
    #[must_use]
    pub fn is_same_participant(&self, other: &Self) -> bool {
        self.guid.prefix == other.guid.prefix
    }

    /// Serialize into a `PL_CDR_LE` parameter list.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::Cdr`] when a value will not encode — in practice only
    /// when a string or the user data is longer than a parameter can hold.
    pub fn to_parameter_list(&self) -> BehaviorResult<ParameterList<'static>> {
        let encoding = Encoding::DISCOVERY;
        let mut list = ParameterList::new(encoding);

        list.push_value(
            ParameterId::new(pid::PROTOCOL_VERSION),
            &self.protocol_version,
        )?;
        list.push_value(ParameterId::new(pid::VENDORID), &self.vendor_id)?;
        if let Some(domain_id) = self.domain_id {
            list.push_value(ParameterId::new(pid::DOMAIN_ID), &domain_id)?;
        }
        if let Some(tag) = &self.domain_tag {
            list.push_value(ParameterId::new(pid::DOMAIN_TAG), tag.as_str())?;
        }
        list.push_value(ParameterId::new(pid::PARTICIPANT_GUID), &self.guid)?;

        for locator in &self.metatraffic_unicast {
            list.push_value(ParameterId::new(pid::METATRAFFIC_UNICAST_LOCATOR), locator)?;
        }
        for locator in &self.metatraffic_multicast {
            list.push_value(
                ParameterId::new(pid::METATRAFFIC_MULTICAST_LOCATOR),
                locator,
            )?;
        }
        for locator in &self.default_unicast {
            list.push_value(ParameterId::new(pid::DEFAULT_UNICAST_LOCATOR), locator)?;
        }
        for locator in &self.default_multicast {
            list.push_value(ParameterId::new(pid::DEFAULT_MULTICAST_LOCATOR), locator)?;
        }

        list.push_value(
            ParameterId::new(pid::BUILTIN_ENDPOINT_SET),
            &self.available_builtin_endpoints,
        )?;
        if self.builtin_endpoint_qos != BuiltinEndpointQos::NONE {
            list.push_value(
                ParameterId::new(pid::BUILTIN_ENDPOINT_QOS),
                &self.builtin_endpoint_qos,
            )?;
        }
        list.push_value(
            ParameterId::new(pid::PARTICIPANT_LEASE_DURATION),
            &self.lease_duration,
        )?;
        list.push_value(
            ParameterId::new(pid::PARTICIPANT_MANUAL_LIVELINESS_COUNT),
            &self.manual_liveliness_count,
        )?;
        if self.expects_inline_qos {
            list.push_value(ParameterId::new(pid::EXPECTS_INLINE_QOS), &true)?;
        }
        if !self.user_data.is_empty() {
            list.push_value(ParameterId::new(pid::USER_DATA), &self.user_data)?;
        }
        if let Some(name) = &self.entity_name {
            list.push_value(ParameterId::new(pid::ENTITY_NAME), name.as_str())?;
        }

        Ok(list)
    }

    /// Serialize into the `SerializedPayload` a `DATA` submessage carries.
    ///
    /// # Errors
    ///
    /// As [`to_parameter_list`](Self::to_parameter_list), plus
    /// [`BehaviorError::Wire`] when the payload will not assemble.
    pub fn to_payload(&self) -> BehaviorResult<SerializedPayload<'static>> {
        let list = self.to_parameter_list()?;
        Ok(SerializedPayload::from_parameter_list(&list)?)
    }

    /// Read an announcement out of a parameter list.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::MissingParameter`] when `PID_PARTICIPANT_GUID` is
    /// absent, and [`BehaviorError::MalformedParameter`] when a parameter is
    /// present but its value will not decode.
    pub fn from_parameter_list(
        list: &ParameterList<'_>,
        encoding: Encoding,
    ) -> BehaviorResult<Self> {
        let guid: Guid = decode_one(list, pid::PARTICIPANT_GUID, encoding, CONTEXT)?.ok_or(
            BehaviorError::MissingParameter {
                context: CONTEXT,
                pid: pid::PARTICIPANT_GUID,
            },
        )?;

        let mut data = Self::new(guid);
        data.protocol_version = decode_one(list, pid::PROTOCOL_VERSION, encoding, CONTEXT)?
            .unwrap_or(ProtocolVersion::CURRENT);
        data.vendor_id =
            decode_one(list, pid::VENDORID, encoding, CONTEXT)?.unwrap_or(VendorId::UNKNOWN);
        data.domain_id = decode_one::<u32>(list, pid::DOMAIN_ID, encoding, CONTEXT)?;
        data.domain_tag =
            decode_bounded_string(list, pid::DOMAIN_TAG, encoding, MAX_DOMAIN_TAG_LEN, CONTEXT)?;

        data.metatraffic_unicast =
            decode_locators(list, pid::METATRAFFIC_UNICAST_LOCATOR, encoding, CONTEXT)?;
        data.metatraffic_multicast =
            decode_locators(list, pid::METATRAFFIC_MULTICAST_LOCATOR, encoding, CONTEXT)?;
        data.default_unicast =
            decode_locators(list, pid::DEFAULT_UNICAST_LOCATOR, encoding, CONTEXT)?;
        data.default_multicast =
            decode_locators(list, pid::DEFAULT_MULTICAST_LOCATOR, encoding, CONTEXT)?;

        data.available_builtin_endpoints =
            decode_one(list, pid::BUILTIN_ENDPOINT_SET, encoding, CONTEXT)?
                .unwrap_or(BuiltinEndpointSet::NONE);
        data.builtin_endpoint_qos = decode_one(list, pid::BUILTIN_ENDPOINT_QOS, encoding, CONTEXT)?
            .unwrap_or(BuiltinEndpointQos::NONE);
        data.lease_duration = decode_one(list, pid::PARTICIPANT_LEASE_DURATION, encoding, CONTEXT)?
            .unwrap_or(DEFAULT_LEASE);
        data.manual_liveliness_count = decode_one(
            list,
            pid::PARTICIPANT_MANUAL_LIVELINESS_COUNT,
            encoding,
            CONTEXT,
        )?
        .unwrap_or(0);
        data.expects_inline_qos =
            decode_one(list, pid::EXPECTS_INLINE_QOS, encoding, CONTEXT)?.unwrap_or(false);
        data.user_data = decode_octets(list, pid::USER_DATA, encoding, MAX_USER_DATA_LEN, CONTEXT)?;
        data.entity_name = decode_bounded_string(
            list,
            pid::ENTITY_NAME,
            encoding,
            MAX_ENTITY_NAME_LEN,
            CONTEXT,
        )?;

        Ok(data)
    }

    /// Read an announcement out of a `DATA` submessage's payload.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::NotAParameterList`] when the encapsulation is not
    /// `PL_CDR`, plus everything
    /// [`from_parameter_list`](Self::from_parameter_list) reports.
    pub fn from_payload(payload: &SerializedPayload<'_>) -> BehaviorResult<Self> {
        if !payload.is_parameter_list() {
            let identifier = payload
                .encapsulation()
                .map_or(0xffff, astrs_cdr::EncapsulationKind::identifier);
            return Err(BehaviorError::NotAParameterList { identifier });
        }
        let (list, encoding) = payload.parameter_list()?;
        Self::from_parameter_list(&list, encoding)
    }
}

impl fmt::Display for ParticipantData {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "participant {} v{} vendor {}",
            self.guid, self.protocol_version, self.vendor_id
        )?;
        if let Some(name) = &self.entity_name {
            write!(formatter, " \"{name}\"")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::discovery::plist::MAX_ANNOUNCED_LOCATORS;
    use crate::structure::{ENTITYID_PARTICIPANT, GuidPrefix, port};
    use std::net::Ipv4Addr;

    fn guid(seed: u8) -> Guid {
        Guid::new(
            GuidPrefix::vendor_scoped(VendorId::ASTRS, [seed; 10]),
            ENTITYID_PARTICIPANT,
        )
    }

    fn announcement() -> ParticipantData {
        ParticipantData::new(guid(1))
            .with_domain(0)
            .with_metatraffic_unicast(Locator::udpv4(Ipv4Addr::LOCALHOST, 45_001))
            .with_metatraffic_multicast(port::default_multicast_locator(0).expect("domain 0 maps"))
            .with_default_unicast(Locator::udpv4(Ipv4Addr::LOCALHOST, 45_002))
            .with_entity_name("talker")
            .with_user_data(b"enclave=/;".to_vec())
            .with_lease(Duration::from_secs(12))
    }

    fn round_trip(data: &ParticipantData) -> ParticipantData {
        let payload = data.to_payload().expect("encode");
        ParticipantData::from_payload(&payload).expect("decode")
    }

    #[test]
    fn a_full_announcement_round_trips() {
        let original = announcement();
        assert_eq!(round_trip(&original), original);
    }

    #[test]
    fn the_payload_is_a_parameter_list_in_little_endian() {
        let payload = announcement().to_payload().expect("encode");
        assert!(payload.is_parameter_list());
        assert_eq!(
            payload.encapsulation(),
            Some(astrs_cdr::EncapsulationKind::PlCdrLe)
        );
        assert_eq!(&payload.as_slice()[..4], &[0x00, 0x03, 0x00, 0x00]);
    }

    #[test]
    fn a_minimal_announcement_round_trips_to_defaults() {
        let minimal = ParticipantData::new(guid(9));
        let decoded = round_trip(&minimal);
        assert_eq!(decoded.guid, guid(9));
        assert_eq!(decoded.lease_duration, DEFAULT_LEASE);
        assert_eq!(decoded.protocol_version, ProtocolVersion::CURRENT);
        assert_eq!(decoded.vendor_id, VendorId::ASTRS);
        assert!(decoded.user_data.is_empty());
        assert_eq!(decoded.entity_name, None);
        assert!(!decoded.expects_inline_qos);
    }

    #[test]
    fn the_guid_is_the_only_required_parameter() {
        let mut list = ParameterList::new(Encoding::DISCOVERY);
        list.push_value(
            ParameterId::new(pid::PROTOCOL_VERSION),
            &ProtocolVersion::V2_3,
        )
        .unwrap();
        let error = ParticipantData::from_parameter_list(&list, Encoding::DISCOVERY)
            .expect_err("must reject");
        assert_eq!(
            error,
            BehaviorError::MissingParameter {
                context: CONTEXT,
                pid: pid::PARTICIPANT_GUID,
            }
        );
    }

    #[test]
    fn unknown_parameters_are_ignored() {
        let mut list = announcement().to_parameter_list().unwrap();
        list.push_octets(ParameterId::new(0x3abc), vec![0xde, 0xad, 0xbe, 0xef])
            .unwrap();
        let decoded = ParticipantData::from_parameter_list(&list, Encoding::DISCOVERY).unwrap();
        assert_eq!(decoded.guid, guid(1));
        assert_eq!(decoded.entity_name.as_deref(), Some("talker"));
    }

    #[test]
    fn a_malformed_locator_is_reported_not_swallowed() {
        let mut list = ParameterList::new(Encoding::DISCOVERY);
        list.push_value(ParameterId::new(pid::PARTICIPANT_GUID), &guid(2))
            .unwrap();
        list.push_octets(
            ParameterId::new(pid::METATRAFFIC_UNICAST_LOCATOR),
            vec![0_u8; 8],
        )
        .unwrap();
        let error = ParticipantData::from_parameter_list(&list, Encoding::DISCOVERY)
            .expect_err("must reject");
        assert!(matches!(
            error,
            BehaviorError::MalformedParameter {
                pid: pid::METATRAFFIC_UNICAST_LOCATOR,
                ..
            }
        ));
    }

    #[test]
    fn too_many_locators_are_refused_before_they_are_stored() {
        let mut list = ParameterList::new(Encoding::DISCOVERY);
        list.push_value(ParameterId::new(pid::PARTICIPANT_GUID), &guid(3))
            .unwrap();
        for port in 0..=(MAX_ANNOUNCED_LOCATORS as u16) {
            list.push_value(
                ParameterId::new(pid::METATRAFFIC_UNICAST_LOCATOR),
                &Locator::udpv4(Ipv4Addr::LOCALHOST, 7400 + port),
            )
            .unwrap();
        }
        let error = ParticipantData::from_parameter_list(&list, Encoding::DISCOVERY)
            .expect_err("must reject");
        assert!(matches!(error, BehaviorError::MalformedParameter { .. }));
    }

    #[test]
    fn repeated_locators_all_survive() {
        let data = ParticipantData::new(guid(4))
            .with_metatraffic_unicast(Locator::udpv4(Ipv4Addr::LOCALHOST, 1))
            .with_metatraffic_unicast(Locator::udpv4(Ipv4Addr::LOCALHOST, 2))
            .with_metatraffic_unicast(Locator::udpv4(Ipv4Addr::LOCALHOST, 3));
        let decoded = round_trip(&data);
        assert_eq!(decoded.metatraffic_unicast.len(), 3);
        assert_eq!(decoded.metatraffic_unicast[2].udp_port(), Some(3));
    }

    #[test]
    fn metatraffic_locators_put_unicast_first() {
        let data = announcement();
        let locators = data.metatraffic_locators();
        assert_eq!(locators.len(), 2);
        assert!(locators[0].is_loopback());
        assert!(locators[1].is_multicast());
        assert!(data.is_reachable());
    }

    #[test]
    fn a_participant_with_no_locators_is_unreachable() {
        assert!(!ParticipantData::new(guid(5)).is_reachable());
    }

    #[test]
    fn oversized_user_data_is_refused() {
        let mut list = ParameterList::new(Encoding::DISCOVERY);
        list.push_value(ParameterId::new(pid::PARTICIPANT_GUID), &guid(6))
            .unwrap();
        list.push_value(
            ParameterId::new(pid::USER_DATA),
            &vec![0_u8; MAX_USER_DATA_LEN + 1],
        )
        .unwrap();
        let error = ParticipantData::from_parameter_list(&list, Encoding::DISCOVERY)
            .expect_err("must reject");
        assert!(matches!(
            error,
            BehaviorError::MalformedParameter {
                pid: pid::USER_DATA,
                ..
            }
        ));
    }

    #[test]
    fn a_payload_that_is_not_a_parameter_list_is_named_as_such() {
        let payload = SerializedPayload::from_cdr(&7_u32).expect("encode");
        let error = ParticipantData::from_payload(&payload).expect_err("must reject");
        assert!(matches!(error, BehaviorError::NotAParameterList { .. }));
    }

    #[test]
    fn the_builtin_endpoint_set_survives() {
        let mut data = ParticipantData::new(guid(7));
        data.available_builtin_endpoints = BuiltinEndpointSet::ROS2_MINIMUM;
        data.builtin_endpoint_qos =
            BuiltinEndpointQos::new(BuiltinEndpointQos::BEST_EFFORT_PARTICIPANT_MESSAGE_READER);
        let decoded = round_trip(&data);
        assert_eq!(
            decoded.available_builtin_endpoints,
            BuiltinEndpointSet::ROS2_MINIMUM
        );
        assert!(
            decoded
                .builtin_endpoint_qos
                .best_effort_participant_message_reader()
        );
    }

    #[test]
    fn expects_inline_qos_round_trips_when_set() {
        let mut data = ParticipantData::new(guid(8));
        data.expects_inline_qos = true;
        assert!(round_trip(&data).expects_inline_qos);
    }

    #[test]
    fn same_participant_ignores_the_entity_id() {
        let left = ParticipantData::new(guid(1));
        let mut right = ParticipantData::new(guid(1));
        right.guid = Guid::new(
            left.guid.prefix,
            crate::structure::ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER,
        );
        assert!(left.is_same_participant(&right));
        assert!(!left.is_same_participant(&ParticipantData::new(guid(2))));
    }

    #[test]
    fn display_names_the_participant() {
        let rendered = announcement().to_string();
        assert!(rendered.contains("talker"), "{rendered}");
        assert!(rendered.contains("2.3"), "{rendered}");
    }

    #[test]
    fn the_domain_tag_round_trips() {
        let mut data = ParticipantData::new(guid(1));
        data.domain_tag = Some("factory-floor".to_owned());
        assert_eq!(
            round_trip(&data).domain_tag.as_deref(),
            Some("factory-floor")
        );
    }
}
