//! SEDP: announcing local endpoints and wiring up the remote ones.
//!
//! Once SPDP has introduced two participants, each announces its endpoints on
//! two reliable, transient-local builtin topics — publications and
//! subscriptions (§8.5.4). This module is the translation layer in both
//! directions:
//!
//! - **Outbound**: a local [`RtpsWriter`] or [`RtpsReader`] becomes a
//!   [`DiscoveredWriterData`] or [`DiscoveredReaderData`] that says where to
//!   reach it and what QoS it offers or requests.
//! - **Inbound**: a remote announcement becomes a [`ReaderProxy`] or
//!   [`WriterProxy`] with the locators and reliability the wiring needs.
//!
//! # The reliability a proxy is built with is the *pair's*, not the peer's
//!
//! This is the subtlety. RTPS runs the reliability protocol when **both**
//! sides asked for it: a `RELIABLE` writer talking to a `BEST_EFFORT` reader
//! must not wait for an ACKNACK that will never come, and a `RELIABLE` reader
//! matched to a `BEST_EFFORT` writer never gets one either — that pairing is
//! refused by request-versus-offered before it reaches this module.
//! [`reader_proxy_for`] and [`writer_proxy_for`] therefore take both QoS sets
//! and use the conjunction.
//!
//! # Builtin endpoints are wired from the mask, not from SEDP
//!
//! The builtin endpoints have fixed GUIDs and fixed QoS, and they must be
//! matched *before* SEDP can run — SEDP is itself one of them. That wiring
//! comes from `PID_BUILTIN_ENDPOINT_SET` in the SPDP announcement, through
//! [`builtin_pairs`](crate::discovery::builtin::builtin_pairs), and
//! [`builtin_reader_proxy`] and [`builtin_writer_proxy`] turn a pair into the
//! two proxies.
//!
//! # Every endpoint is an instance of its own
//!
//! Both SEDP topics are keyed: the key of a sample is the endpoint's GUID
//! (§8.5.4.2), and a key of sixteen octets is its own key hash (§9.6.3.8).
//! The builtin writers keep `KEEP_LAST 1` under `TRANSIENT_LOCAL`, and
//! `KEEP_LAST` counts *per instance* — so an announcement, and the disposal
//! that later retires it, are filed under the endpoint's GUID and never under
//! the keyless `InstanceHandle::NIL`. Shared by every endpoint, that one
//! instance would hold only the newest announcement, and a peer that
//! discovers this participant late would be replayed a single endpoint per
//! topic. The receiving side files what arrives under the same GUID, read
//! from `PID_KEY_HASH` or from the sample itself, so a reader's `KEEP_LAST 1`
//! is one announcement per endpoint as well. SPDP is keyed the same way, by
//! the participant's GUID.

use astrs_cdr::{ParameterList, pid};

use crate::behavior::cache::{ChangeKind, INSTANCE_HANDLE_LEN, InstanceHandle};
use crate::behavior::endpoint::TopicKey;
use crate::behavior::error::BehaviorResult;
use crate::behavior::proxy::{ReaderProxy, WriterProxy};
use crate::behavior::reader::RtpsReader;
use crate::behavior::writer::RtpsWriter;
use crate::discovery::builtin::BuiltinPair;
use crate::discovery::compat::RosCompat;
use crate::discovery::endpoint_data::{
    DiscoveredReaderData, DiscoveredWriterData, EndpointIdentity, XCDR1_REPRESENTATION,
    XCDR2_REPRESENTATION,
};
use crate::discovery::matching::{ReaderQos, WriterQos};
use crate::discovery::plist::decode_one;
use crate::messages::SerializedPayload;
use crate::structure::{
    ENTITYID_SEDP_BUILTIN_PUBLICATIONS_READER, ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_READER,
    ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER, EntityId, GUID_LEN, Guid, Locator,
};

/// Build the SEDP publication sample for a local writer.
///
/// The endpoint announces no locators of its own: an AstRS participant has
/// one user-traffic socket, and a peer resolves an endpoint with no locators
/// to the participant's `PID_DEFAULT_UNICAST_LOCATOR`. Passing an explicit
/// list is still supported for a deployment that wants per-endpoint sockets.
///
/// # Errors
///
/// [`BehaviorError::EmptyName`](crate::behavior::BehaviorError::EmptyName) or
/// [`BehaviorError::NameTooLong`](crate::behavior::BehaviorError::NameTooLong)
/// when the writer's topic names are unusable — which
/// [`TopicKey::new`](crate::behavior::endpoint::TopicKey::new) already
/// refused, so in practice this cannot fire.
pub fn publication_for(
    writer: &RtpsWriter,
    locators: Vec<Locator>,
    compat: RosCompat,
) -> BehaviorResult<DiscoveredWriterData> {
    let identity = identity_for(writer.guid(), writer.topic(), locators, compat)?;
    Ok(DiscoveredWriterData::new(
        writer.guid(),
        &writer.topic().topic_name,
        &writer.topic().type_name,
    )?
    .with_identity(identity)
    .with_qos(*writer.qos()))
}

/// Build the SEDP subscription sample for a local reader.
///
/// # Errors
///
/// As [`publication_for`].
pub fn subscription_for(
    reader: &RtpsReader,
    locators: Vec<Locator>,
    compat: RosCompat,
) -> BehaviorResult<DiscoveredReaderData> {
    let identity = identity_for(reader.guid(), reader.topic(), locators, compat)?;
    let mut data = DiscoveredReaderData::new(
        reader.guid(),
        &reader.topic().topic_name,
        &reader.topic().type_name,
    )?
    .with_identity(identity)
    .with_qos(*reader.qos());
    data.expects_inline_qos = reader.config().expects_inline_qos;
    Ok(data)
}

/// The shared identity half of both samples.
///
/// # Errors
///
/// As [`publication_for`].
fn identity_for(
    guid: Guid,
    topic: &TopicKey,
    locators: Vec<Locator>,
    compat: RosCompat,
) -> BehaviorResult<EndpointIdentity> {
    let mut identity = EndpointIdentity::new(guid, &topic.topic_name, &topic.type_name)?;
    identity.unicast = locators;
    identity.data_representation = if compat.advertises_xcdr2() {
        vec![XCDR1_REPRESENTATION, XCDR2_REPRESENTATION]
    } else {
        vec![XCDR1_REPRESENTATION]
    };
    Ok(identity)
}

/// The proxy a local writer keeps for a remote reader it has matched.
///
/// `offered` is the local writer's QoS; the pairing is reliable only when
/// both sides asked for it.
#[must_use]
pub fn reader_proxy_for(
    remote: &DiscoveredReaderData,
    offered: &WriterQos,
    locators: Vec<Locator>,
) -> ReaderProxy {
    let reliable = offered.is_reliable() && remote.qos.is_reliable();
    ReaderProxy::new(remote.guid(), locators, reliable)
        .expecting_inline_qos(remote.expects_inline_qos)
        // The remote reader's own `DURABILITY`. A `VOLATILE` reader is not
        // replayed the writer's history even when the writer is
        // `TRANSIENT_LOCAL` and still holds it — see
        // `RtpsWriter::match_reader`.
        .wanting_history(remote.qos.wants_history())
}

/// The proxy a local reader keeps for a remote writer it has matched.
///
/// `requested` is the local reader's QoS.
#[must_use]
pub fn writer_proxy_for(
    remote: &DiscoveredWriterData,
    requested: &ReaderQos,
    locators: Vec<Locator>,
) -> WriterProxy {
    let reliable = requested.is_reliable() && remote.qos.is_reliable();
    WriterProxy::new(remote.guid(), locators, reliable)
}

/// The proxy a builtin writer keeps for its counterpart on a peer.
#[must_use]
pub fn builtin_reader_proxy(pair: BuiltinPair, locators: Vec<Locator>) -> ReaderProxy {
    ReaderProxy::new(pair.reader, locators, pair.reliable)
}

/// The proxy a builtin reader keeps for its counterpart on a peer.
#[must_use]
pub fn builtin_writer_proxy(pair: BuiltinPair, locators: Vec<Locator>) -> WriterProxy {
    WriterProxy::new(pair.writer, locators, pair.reliable)
}

/// The instance a sample on a GUID-keyed discovery topic is filed under.
///
/// The key of `DCPSPublication` and `DCPSSubscription` is the endpoint's
/// GUID, and that of `DCPSParticipant` the participant's. A key of at most
/// sixteen octets is its own key hash (§9.6.3.8), so the GUID's octets *are*
/// the instance handle.
#[must_use]
pub(crate) const fn guid_instance(guid: Guid) -> InstanceHandle {
    InstanceHandle::new(guid.to_bytes())
}

/// The instance a sample arriving at the local reader `reader` belongs to.
///
/// [`InstanceHandle::NIL`] unless `reader` is one of the three discovery
/// readers whose topic is keyed by a GUID. For those, in §9.6.3.8's order —
/// a key hash the writer sent is authoritative, and one it did not send is
/// computed from the sample:
///
/// 1. `PID_KEY_HASH` in the inline QoS, when it is there and sixteen octets.
/// 2. The GUID parameter of a `PL_CDR` payload: every announcement, and a
///    disposal whose key is sent as a parameter list.
/// 3. For a disposal, the GUID a plain-CDR key carries — the shape
///    [`RtpsWriter::dispose`] sends, read by [`guid_from_key`].
///
/// So an announcement and the disposal that retires it land on the same
/// instance whichever form a peer used. A sample none of the three can
/// attribute stays on NIL: it is still delivered, merely not told apart.
#[must_use]
pub(crate) fn builtin_instance(
    reader: EntityId,
    kind: ChangeKind,
    inline_qos: Option<&ParameterList<'_>>,
    payload: &[u8],
) -> InstanceHandle {
    let Some(parameter) = key_parameter(reader) else {
        return InstanceHandle::NIL;
    };
    if let Some(instance) = inline_qos.and_then(key_hash) {
        return instance;
    }
    let payload = SerializedPayload::new(payload);
    let guid = if payload.is_parameter_list() {
        payload.parameter_list().ok().and_then(|(list, encoding)| {
            decode_one::<Guid>(&list, parameter, encoding, "discovery sample key")
                .ok()
                .flatten()
        })
    } else if kind.carries_data() {
        // A live discovery sample is always a parameter list. Anything else
        // is refused when it is absorbed, and names no key to file it under.
        None
    } else {
        guid_from_key(payload.as_slice())
    };
    guid.map_or(InstanceHandle::NIL, guid_instance)
}

/// The GUID a discovery sample's instance names, or `None` for
/// [`InstanceHandle::NIL`].
///
/// The inverse of [`guid_instance`]: on the GUID-keyed discovery topics the
/// instance handle is the key hash, and the key hash is the GUID. So a
/// sample [`builtin_instance`] attributed — by `PID_KEY_HASH`, by a `PL_CDR`
/// key or by a plain-CDR one — names its entity here, whichever form the
/// peer sent. A disposal that carries only a key hash, which DDSI-RTPS
/// §9.6.3.8 allows, has no payload to read a GUID from at all.
#[must_use]
pub(crate) const fn guid_of_instance(instance: InstanceHandle) -> Option<Guid> {
    if instance.is_nil() {
        None
    } else {
        Some(Guid::from_bytes(*instance.as_bytes()))
    }
}

/// Read a GUID out of a disposal sample's key payload.
///
/// A `DATA` that disposes of a builtin instance carries the key rather than
/// the sample, and for the discovery topics the key *is* the entity's GUID:
/// sixteen octets after the four-octet encapsulation header, which is what
/// [`RtpsWriter::dispose`] sends. Anything else is a disposal this reading
/// cannot attribute, and is ignored rather than guessed at.
pub(crate) fn guid_from_key(payload: &[u8]) -> Option<Guid> {
    let body = payload.get(astrs_cdr::ENCAPSULATION_HEADER_LEN..)?;
    Guid::from_slice(body.get(..GUID_LEN)?)
}

/// The parameter that carries the key of the topic `reader` subscribes to,
/// or `None` when that topic is not keyed by a GUID.
///
/// Three builtin readers qualify. The WLP reader's key is a prefix and a
/// kind rather than a GUID, and a user topic's key belongs to its type —
/// every ROS 2 topic is keyless — so both stay on [`InstanceHandle::NIL`].
fn key_parameter(reader: EntityId) -> Option<u16> {
    if reader == ENTITYID_SEDP_BUILTIN_PUBLICATIONS_READER
        || reader == ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_READER
    {
        Some(pid::ENDPOINT_GUID)
    } else if reader == ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER {
        Some(pid::PARTICIPANT_GUID)
    } else {
        None
    }
}

/// A sixteen-octet `PID_KEY_HASH` out of an inline QoS list.
fn key_hash(inline_qos: &ParameterList<'_>) -> Option<InstanceHandle> {
    let parameter = inline_qos.get_by_base(pid::KEY_HASH)?;
    let octets = <[u8; INSTANCE_HANDLE_LEN]>::try_from(parameter.value.as_ref()).ok()?;
    Some(InstanceHandle::new(octets))
}

/// The QoS every SEDP builtin writer offers.
#[must_use]
pub fn sedp_writer_qos() -> WriterQos {
    WriterQos::builtin_sedp()
}

/// The QoS every SEDP builtin reader requests.
#[must_use]
pub fn sedp_reader_qos() -> ReaderQos {
    ReaderQos::builtin_sedp()
}

/// The DDS topic name of the SEDP publications topic.
pub const PUBLICATIONS_TOPIC: &str = "DCPSPublication";

/// The DDS type name of the SEDP publications topic.
pub const PUBLICATIONS_TYPE: &str = "PublicationBuiltinTopicData";

/// The DDS topic name of the SEDP subscriptions topic.
pub const SUBSCRIPTIONS_TOPIC: &str = "DCPSSubscription";

/// The DDS type name of the SEDP subscriptions topic.
pub const SUBSCRIPTIONS_TYPE: &str = "SubscriptionBuiltinTopicData";

/// The DDS topic name of the SPDP participant topic.
pub const PARTICIPANT_TOPIC: &str = "DCPSParticipant";

/// The DDS type name of the SPDP participant topic.
pub const PARTICIPANT_TYPE: &str = "ParticipantBuiltinTopicData";

/// The topic of the SEDP publications builtin endpoint.
///
/// # Errors
///
/// Cannot fail; the names are constants this crate controls.
pub fn publications_topic() -> BehaviorResult<TopicKey> {
    TopicKey::new(PUBLICATIONS_TOPIC, PUBLICATIONS_TYPE)
}

/// The topic of the SEDP subscriptions builtin endpoint.
///
/// # Errors
///
/// Cannot fail.
pub fn subscriptions_topic() -> BehaviorResult<TopicKey> {
    TopicKey::new(SUBSCRIPTIONS_TOPIC, SUBSCRIPTIONS_TYPE)
}

/// The topic of the SPDP participant builtin endpoint.
///
/// # Errors
///
/// Cannot fail.
pub fn participant_topic() -> BehaviorResult<TopicKey> {
    TopicKey::new(PARTICIPANT_TOPIC, PARTICIPANT_TYPE)
}

/// The topic of the WLP participant-message builtin endpoint.
///
/// # Errors
///
/// Cannot fail.
pub fn participant_message_topic() -> BehaviorResult<TopicKey> {
    TopicKey::new(
        crate::behavior::liveliness::WLP_TOPIC_NAME,
        crate::behavior::liveliness::WLP_TYPE_NAME,
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::behavior::reader::ReaderConfig;
    use crate::behavior::writer::WriterConfig;
    use crate::discovery::builtin::{BuiltinEndpointSet, builtin_pairs};
    use crate::discovery::participant_data::ParticipantData;
    use crate::messages::inline_qos_encoding;
    use crate::structure::{
        ENTITYID_P2P_BUILTIN_PARTICIPANT_MESSAGE_READER, ENTITYID_PARTICIPANT,
        ENTITYID_SEDP_BUILTIN_PUBLICATIONS_WRITER, EntityId, EntityKind, GuidPrefix, VendorId,
    };
    use astrs_cdr::{Encoding, Endianness, ParameterId};
    use std::net::Ipv4Addr;
    use std::time::Instant;

    const TOPIC: &str = "rt/chatter";
    const TYPE: &str = "std_msgs::msg::dds_::String_";

    fn prefix(seed: u8) -> GuidPrefix {
        GuidPrefix::vendor_scoped(VendorId::ASTRS, [seed; 10])
    }

    fn topic() -> TopicKey {
        TopicKey::new(TOPIC, TYPE).unwrap()
    }

    fn writer(qos: WriterQos) -> RtpsWriter {
        RtpsWriter::new(
            WriterConfig::new(
                Guid::new(
                    prefix(1),
                    EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY),
                ),
                topic(),
            )
            .with_qos(qos),
        )
    }

    fn reader(qos: ReaderQos) -> RtpsReader {
        RtpsReader::new(
            ReaderConfig::new(
                Guid::new(
                    prefix(2),
                    EntityId::user_defined(1, EntityKind::USER_READER_NO_KEY),
                ),
                topic(),
            )
            .with_qos(qos),
        )
    }

    fn locator(port: u16) -> Locator {
        Locator::udpv4(Ipv4Addr::LOCALHOST, port)
    }

    #[test]
    fn a_publication_sample_carries_the_writers_guid_topic_and_qos() {
        let writer = writer(WriterQos::latched(7));
        let sample = publication_for(&writer, vec![locator(45_001)], RosCompat::Jazzy).unwrap();
        assert_eq!(sample.guid(), writer.guid());
        assert_eq!(sample.identity.topic_name, TOPIC);
        assert_eq!(sample.identity.type_name, TYPE);
        assert_eq!(sample.qos, WriterQos::latched(7));
        assert_eq!(sample.identity.unicast, vec![locator(45_001)]);
    }

    #[test]
    fn a_subscription_sample_carries_the_readers_guid_topic_and_qos() {
        let reader = reader(ReaderQos::reliable(3));
        let sample = subscription_for(&reader, vec![locator(45_002)], RosCompat::Humble).unwrap();
        assert_eq!(sample.guid(), reader.guid());
        assert_eq!(sample.qos, ReaderQos::reliable(3));
        assert!(!sample.expects_inline_qos);
    }

    #[test]
    fn jazzy_advertises_xcdr2_and_humble_does_not() {
        let writer = writer(WriterQos::default());
        let jazzy = publication_for(&writer, Vec::new(), RosCompat::Jazzy).unwrap();
        assert!(jazzy.identity.accepts_xcdr2());
        assert_eq!(
            jazzy.identity.data_representation,
            vec![XCDR1_REPRESENTATION, XCDR2_REPRESENTATION]
        );

        let humble = publication_for(&writer, Vec::new(), RosCompat::Humble).unwrap();
        assert!(!humble.identity.accepts_xcdr2());
        assert_eq!(
            humble.identity.data_representation,
            vec![XCDR1_REPRESENTATION]
        );
    }

    #[test]
    fn a_sample_with_no_locators_round_trips_and_falls_back() {
        let writer = writer(WriterQos::default());
        let sample = publication_for(&writer, Vec::new(), RosCompat::Jazzy).unwrap();
        let decoded = DiscoveredWriterData::from_payload(&sample.to_payload().unwrap()).unwrap();
        assert_eq!(decoded, sample);
        assert_eq!(
            decoded.identity.resolve_locators(&[locator(46_000)]),
            vec![locator(46_000)],
            "no endpoint locator means the participant's default"
        );
    }

    #[test]
    fn a_pairing_is_reliable_only_when_both_sides_are() {
        let reliable_reader = subscription_for(
            &reader(ReaderQos::reliable(1)),
            Vec::new(),
            RosCompat::Jazzy,
        )
        .unwrap();
        let best_effort_reader = subscription_for(
            &reader(ReaderQos::sensor_data()),
            Vec::new(),
            RosCompat::Jazzy,
        )
        .unwrap();

        assert!(
            reader_proxy_for(&reliable_reader, &WriterQos::services_default(), Vec::new())
                .is_reliable()
        );
        assert!(
            !reader_proxy_for(
                &best_effort_reader,
                &WriterQos::services_default(),
                Vec::new()
            )
            .is_reliable(),
            "a best-effort reader will never ACKNACK"
        );
        assert!(
            !reader_proxy_for(&reliable_reader, &WriterQos::sensor_data(), Vec::new())
                .is_reliable(),
            "a best-effort writer has nothing to retransmit from"
        );
    }

    #[test]
    fn the_same_conjunction_governs_the_writer_proxy() {
        let reliable_writer = publication_for(
            &writer(WriterQos::services_default()),
            Vec::new(),
            RosCompat::Jazzy,
        )
        .unwrap();
        let best_effort_writer = publication_for(
            &writer(WriterQos::sensor_data()),
            Vec::new(),
            RosCompat::Jazzy,
        )
        .unwrap();

        assert!(
            writer_proxy_for(&reliable_writer, &ReaderQos::reliable(1), Vec::new()).is_reliable()
        );
        assert!(
            !writer_proxy_for(&best_effort_writer, &ReaderQos::reliable(1), Vec::new())
                .is_reliable()
        );
        assert!(
            !writer_proxy_for(&reliable_writer, &ReaderQos::sensor_data(), Vec::new())
                .is_reliable()
        );
    }

    #[test]
    fn a_reader_that_wants_inline_qos_says_so() {
        let mut reader = reader(ReaderQos::reliable(1));
        // The flag lives on the config, so rebuild with it set.
        reader = RtpsReader::new(ReaderConfig {
            expects_inline_qos: true,
            ..reader.config().clone()
        });
        let sample = subscription_for(&reader, Vec::new(), RosCompat::Jazzy).unwrap();
        assert!(sample.expects_inline_qos);
        let proxy = reader_proxy_for(&sample, &WriterQos::default(), Vec::new());
        assert!(proxy.expects_inline_qos());
    }

    #[test]
    fn builtin_proxies_take_their_reliability_from_the_pair() {
        let pairs = builtin_pairs(
            prefix(1),
            BuiltinEndpointSet::ASTRS,
            prefix(2),
            BuiltinEndpointSet::ASTRS,
        );
        let spdp_pair = pairs
            .iter()
            .find(|pair| !pair.reliable)
            .copied()
            .expect("SPDP is the best-effort one");
        let sedp_pair = pairs
            .iter()
            .find(|pair| pair.reliable)
            .copied()
            .expect("SEDP is reliable");

        assert!(!builtin_reader_proxy(spdp_pair, vec![locator(1)]).is_reliable());
        assert!(builtin_reader_proxy(sedp_pair, vec![locator(1)]).is_reliable());
        assert!(!builtin_writer_proxy(spdp_pair, vec![locator(1)]).is_reliable());
        assert!(builtin_writer_proxy(sedp_pair, vec![locator(1)]).is_reliable());
        assert_eq!(
            builtin_reader_proxy(sedp_pair, vec![locator(1)]).guid(),
            sedp_pair.reader
        );
    }

    #[test]
    fn the_builtin_qos_profiles_pair_up() {
        assert_eq!(
            crate::discovery::matching::check_qos(&sedp_reader_qos(), &sedp_writer_qos()),
            Ok(())
        );
        assert!(sedp_writer_qos().is_reliable());
        assert!(sedp_writer_qos().replays_history());
    }

    #[test]
    fn the_builtin_topic_names_are_the_dds_ones() {
        assert_eq!(publications_topic().unwrap().topic_name, "DCPSPublication");
        assert_eq!(
            publications_topic().unwrap().type_name,
            "PublicationBuiltinTopicData"
        );
        assert_eq!(
            subscriptions_topic().unwrap().topic_name,
            "DCPSSubscription"
        );
        assert_eq!(participant_topic().unwrap().topic_name, "DCPSParticipant");
        assert_eq!(
            participant_message_topic().unwrap().topic_name,
            "DCPSParticipantMessage"
        );
    }

    #[test]
    fn a_publication_and_a_subscription_on_one_topic_match() {
        let publication = publication_for(
            &writer(WriterQos::services_default()),
            Vec::new(),
            RosCompat::Jazzy,
        )
        .unwrap();
        let subscription = subscription_for(
            &reader(ReaderQos::reliable(10)),
            Vec::new(),
            RosCompat::Jazzy,
        )
        .unwrap();
        let outcome = crate::discovery::matching::match_endpoints(
            &subscription.identity.topic_name,
            &subscription.identity.type_name,
            &subscription.qos,
            &publication.identity.topic_name,
            &publication.identity.type_name,
            &publication.qos,
        );
        assert!(outcome.is_matched());
    }

    // -----------------------------------------------------------------------
    // Instances: every discovery sample is filed under the GUID it names
    // -----------------------------------------------------------------------

    /// Where a live sample with no inline QoS is filed.
    fn filed(reader: EntityId, payload: &[u8]) -> InstanceHandle {
        builtin_instance(reader, ChangeKind::Alive, None, payload)
    }

    /// An inline QoS list carrying one `PID_KEY_HASH` of `octets`.
    fn key_hash_qos(octets: &[u8]) -> ParameterList<'static> {
        let mut list = ParameterList::new(inline_qos_encoding(Endianness::Little));
        list.push_octets(ParameterId::new(pid::KEY_HASH), octets.to_vec())
            .unwrap();
        list
    }

    fn publication_payload(writer: &RtpsWriter) -> Vec<u8> {
        publication_for(writer, Vec::new(), RosCompat::Jazzy)
            .unwrap()
            .to_payload()
            .unwrap()
            .into_cow()
            .into_owned()
    }

    #[test]
    fn every_discovery_sample_is_filed_under_the_guid_it_names() {
        let publisher = writer(WriterQos::default());
        assert_eq!(
            filed(
                ENTITYID_SEDP_BUILTIN_PUBLICATIONS_READER,
                &publication_payload(&publisher)
            ),
            guid_instance(publisher.guid())
        );

        let subscriber = reader(ReaderQos::default());
        let subscription = subscription_for(&subscriber, Vec::new(), RosCompat::Jazzy)
            .unwrap()
            .to_payload()
            .unwrap();
        assert_eq!(
            filed(
                ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_READER,
                subscription.as_slice()
            ),
            guid_instance(subscriber.guid())
        );

        let participant = prefix(3).with_entity(ENTITYID_PARTICIPANT);
        let announcement = ParticipantData::new(participant).to_payload().unwrap();
        assert_eq!(
            filed(
                ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER,
                announcement.as_slice()
            ),
            guid_instance(participant)
        );
    }

    #[test]
    fn an_announcement_and_its_disposal_share_an_instance() {
        let publisher = writer(WriterQos::default());
        // The disposal exactly as the SEDP writer writes it.
        let mut announcer = RtpsWriter::new(
            WriterConfig::new(
                Guid::new(prefix(1), ENTITYID_SEDP_BUILTIN_PUBLICATIONS_WRITER),
                publications_topic().unwrap(),
            )
            .with_qos(WriterQos::builtin_sedp()),
        );
        let number = announcer.dispose(publisher.guid(), Instant::now()).unwrap();
        let change = announcer.cache().get(number).unwrap();
        assert_eq!(
            change.instance,
            guid_instance(publisher.guid()),
            "the writer files the disposal under the endpoint it names"
        );
        assert_eq!(
            builtin_instance(
                ENTITYID_SEDP_BUILTIN_PUBLICATIONS_READER,
                ChangeKind::NotAliveDisposed,
                None,
                &change.payload,
            ),
            filed(
                ENTITYID_SEDP_BUILTIN_PUBLICATIONS_READER,
                &publication_payload(&publisher)
            ),
            "and a reader files it where it filed the announcement"
        );
    }

    #[test]
    fn a_disposal_keyed_by_a_parameter_list_is_attributed_too() {
        // How other stacks send a disposal's key: a `PL_CDR` list holding
        // `PID_ENDPOINT_GUID` alone.
        let endpoint = writer(WriterQos::default()).guid();
        let mut list = ParameterList::new(Encoding::DISCOVERY);
        list.push_value(ParameterId::new(pid::ENDPOINT_GUID), &endpoint)
            .unwrap();
        let key = SerializedPayload::from_parameter_list(&list).unwrap();
        assert_eq!(
            builtin_instance(
                ENTITYID_SEDP_BUILTIN_PUBLICATIONS_READER,
                ChangeKind::NotAliveDisposed,
                None,
                key.as_slice(),
            ),
            guid_instance(endpoint)
        );
    }

    #[test]
    fn a_key_hash_outranks_the_sample_when_it_is_sixteen_octets() {
        let publisher = writer(WriterQos::default());
        let payload = publication_payload(&publisher);
        let reader = ENTITYID_SEDP_BUILTIN_PUBLICATIONS_READER;
        assert_eq!(
            builtin_instance(
                reader,
                ChangeKind::Alive,
                Some(&key_hash_qos(&[5; 16])),
                &payload
            ),
            InstanceHandle::new([5; 16]),
            "a key hash the writer sent is authoritative"
        );
        assert_eq!(
            builtin_instance(
                reader,
                ChangeKind::Alive,
                Some(&key_hash_qos(&[5; 8])),
                &payload
            ),
            guid_instance(publisher.guid()),
            "a key hash of the wrong length is none, and the sample decides"
        );
    }

    #[test]
    fn only_the_guid_keyed_discovery_readers_file_by_key() {
        let payload = publication_payload(&writer(WriterQos::default()));
        let key_hash = key_hash_qos(&[5; 16]);
        for reader in [
            ENTITYID_P2P_BUILTIN_PARTICIPANT_MESSAGE_READER,
            EntityId::user_defined(1, EntityKind::USER_READER_NO_KEY),
            EntityId::user_defined(1, EntityKind::USER_READER_WITH_KEY),
        ] {
            assert_eq!(
                builtin_instance(reader, ChangeKind::Alive, Some(&key_hash), &payload),
                InstanceHandle::NIL,
                "{reader:?} is not keyed by a GUID"
            );
        }
    }

    #[test]
    fn a_sample_that_names_no_guid_stays_on_nil() {
        let reader = ENTITYID_SEDP_BUILTIN_PUBLICATIONS_READER;
        assert_eq!(
            builtin_instance(reader, ChangeKind::NotAliveDisposed, None, &[]),
            InstanceHandle::NIL,
            "an empty disposal"
        );

        // A GUID, but not the parameter that keys this topic.
        let mut list = ParameterList::new(Encoding::DISCOVERY);
        list.push_value(
            ParameterId::new(pid::PARTICIPANT_GUID),
            &prefix(3).with_entity(ENTITYID_PARTICIPANT),
        )
        .unwrap();
        let wrong = SerializedPayload::from_parameter_list(&list).unwrap();
        assert_eq!(filed(reader, wrong.as_slice()), InstanceHandle::NIL);

        // A live sample that is not a parameter list is refused when it is
        // absorbed; it is not read as a key either.
        let mut raw = vec![0x00, 0x01, 0x00, 0x00];
        raw.extend_from_slice(&writer(WriterQos::default()).guid().to_bytes());
        assert_eq!(filed(reader, &raw), InstanceHandle::NIL);

        assert_eq!(guid_from_key(&[0x00, 0x01, 0x00, 0x00, 1, 2, 3]), None);
    }
}
