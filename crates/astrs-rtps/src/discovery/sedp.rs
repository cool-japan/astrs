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
use crate::structure::{Guid, Locator};

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
    use crate::structure::{EntityId, EntityKind, GuidPrefix, VendorId};
    use std::net::Ipv4Addr;

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
}
