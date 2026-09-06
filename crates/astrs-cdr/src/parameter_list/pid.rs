//! Standard RTPS and DDS parameter ids.
//!
//! The `PID_*` values of OMG DDSI-RTPS 2.3 §9.6.2.2 (Table 9.14 and the
//! discovery tables of §9.6.3), which is the vocabulary SPDP and SEDP
//! discovery samples are written in. `astrs-rtps` builds every discovery
//! sample out of these; nothing in this crate interprets them, so a parameter
//! id this table does not name is carried through unchanged rather than
//! rejected.
//!
//! # The id space
//!
//! A parameter id is sixteen bits with two flags in the top:
//!
//! - [`FLAG_VENDOR_SPECIFIC`] (bit 15) — the parameter is defined by a
//!   vendor rather than by the specification.
//! - [`FLAG_MUST_UNDERSTAND`] (bit 14) — a receiver that does not recognise
//!   the parameter must discard the whole sample.
//!
//! [`ParameterId::base`](super::ParameterId::base) strips both, so a lookup
//! can match on the id a table names regardless of how a sender flagged it.
//!
//! Two ids are structural rather than data-carrying:
//! [`SENTINEL`] terminates every list, and [`PAD`] is an ignorable filler
//! entry.

/// Bit 15 of a parameter id: the parameter is vendor-specific.
pub const FLAG_VENDOR_SPECIFIC: u16 = 0x8000;

/// Bit 14 of a parameter id: a receiver that does not recognise the parameter
/// must discard the sample.
pub const FLAG_MUST_UNDERSTAND: u16 = 0x4000;

/// Mask of the id itself, both flags removed.
pub const BASE_MASK: u16 = 0x3fff;

/// `PID_PAD` — an ignorable filler entry.
pub const PAD: u16 = 0x0000;
/// `PID_SENTINEL` — terminates a parameter list. Its length is zero and it
/// carries no value.
pub const SENTINEL: u16 = 0x0001;

/// `PID_PARTICIPANT_LEASE_DURATION` — how long a participant may go silent
/// before peers consider it gone.
pub const PARTICIPANT_LEASE_DURATION: u16 = 0x0002;
/// `PID_TIME_BASED_FILTER` — minimum separation QoS.
pub const TIME_BASED_FILTER: u16 = 0x0004;
/// `PID_TOPIC_NAME` — the DDS topic name, e.g. `rt/scan`.
pub const TOPIC_NAME: u16 = 0x0005;
/// `PID_OWNERSHIP_STRENGTH` — exclusive-ownership tie-break.
pub const OWNERSHIP_STRENGTH: u16 = 0x0006;
/// `PID_TYPE_NAME` — the DDS type name, e.g.
/// `sensor_msgs::msg::dds_::LaserScan_`.
pub const TYPE_NAME: u16 = 0x0007;

/// `PID_METATRAFFIC_MULTICAST_IPADDRESS` — legacy IPv4 discovery address.
pub const METATRAFFIC_MULTICAST_IPADDRESS: u16 = 0x000b;
/// `PID_DEFAULT_UNICAST_IPADDRESS` — legacy IPv4 user-traffic address.
pub const DEFAULT_UNICAST_IPADDRESS: u16 = 0x000c;
/// `PID_METATRAFFIC_UNICAST_PORT` — legacy discovery port.
pub const METATRAFFIC_UNICAST_PORT: u16 = 0x000d;
/// `PID_DEFAULT_UNICAST_PORT` — legacy user-traffic port.
pub const DEFAULT_UNICAST_PORT: u16 = 0x000e;
/// `PID_MULTICAST_IPADDRESS` — legacy IPv4 multicast address.
pub const MULTICAST_IPADDRESS: u16 = 0x0011;

/// `PID_PROTOCOL_VERSION` — the RTPS version the participant speaks.
pub const PROTOCOL_VERSION: u16 = 0x0015;
/// `PID_VENDORID` — the two-octet OMG vendor identifier.
pub const VENDORID: u16 = 0x0016;
/// `PID_RELIABILITY` — the reliability QoS policy.
pub const RELIABILITY: u16 = 0x001a;
/// `PID_LIVELINESS` — the liveliness QoS policy.
pub const LIVELINESS: u16 = 0x001b;
/// `PID_DURABILITY` — volatile, transient-local, transient or persistent.
pub const DURABILITY: u16 = 0x001d;
/// `PID_DURABILITY_SERVICE` — the durability-service QoS policy.
pub const DURABILITY_SERVICE: u16 = 0x001e;
/// `PID_OWNERSHIP` — shared or exclusive.
pub const OWNERSHIP: u16 = 0x001f;
/// `PID_PRESENTATION` — access-scope and ordering QoS.
pub const PRESENTATION: u16 = 0x0021;
/// `PID_DEADLINE` — the deadline QoS policy.
pub const DEADLINE: u16 = 0x0023;
/// `PID_DESTINATION_ORDER` — by-reception-timestamp or by-source-timestamp.
pub const DESTINATION_ORDER: u16 = 0x0025;
/// `PID_LATENCY_BUDGET` — the latency-budget QoS policy.
pub const LATENCY_BUDGET: u16 = 0x0027;
/// `PID_PARTITION` — the partition QoS policy.
pub const PARTITION: u16 = 0x0029;
/// `PID_LIFESPAN` — how long a sample stays valid.
pub const LIFESPAN: u16 = 0x002b;
/// `PID_USER_DATA` — opaque application data on a participant or endpoint.
pub const USER_DATA: u16 = 0x002c;
/// `PID_GROUP_DATA` — opaque application data on a publisher or subscriber.
pub const GROUP_DATA: u16 = 0x002d;
/// `PID_TOPIC_DATA` — opaque application data on a topic.
pub const TOPIC_DATA: u16 = 0x002e;
/// `PID_UNICAST_LOCATOR` — an endpoint's unicast locator.
pub const UNICAST_LOCATOR: u16 = 0x002f;
/// `PID_MULTICAST_LOCATOR` — an endpoint's multicast locator.
pub const MULTICAST_LOCATOR: u16 = 0x0030;
/// `PID_DEFAULT_UNICAST_LOCATOR` — a participant's default user-traffic
/// unicast locator.
pub const DEFAULT_UNICAST_LOCATOR: u16 = 0x0031;
/// `PID_METATRAFFIC_UNICAST_LOCATOR` — a participant's discovery unicast
/// locator.
pub const METATRAFFIC_UNICAST_LOCATOR: u16 = 0x0032;
/// `PID_METATRAFFIC_MULTICAST_LOCATOR` — a participant's discovery multicast
/// locator.
pub const METATRAFFIC_MULTICAST_LOCATOR: u16 = 0x0033;
/// `PID_PARTICIPANT_MANUAL_LIVELINESS_COUNT` — the manual liveliness
/// counter.
pub const PARTICIPANT_MANUAL_LIVELINESS_COUNT: u16 = 0x0034;
/// `PID_CONTENT_FILTER_PROPERTY` — content-filtered-topic description.
pub const CONTENT_FILTER_PROPERTY: u16 = 0x0035;

/// `PID_HISTORY` — keep-last or keep-all, with depth.
pub const HISTORY: u16 = 0x0040;
/// `PID_RESOURCE_LIMITS` — the resource-limits QoS policy.
pub const RESOURCE_LIMITS: u16 = 0x0041;
/// `PID_EXPECTS_INLINE_QOS` — whether a reader wants inline QoS on data.
pub const EXPECTS_INLINE_QOS: u16 = 0x0043;
/// `PID_PARTICIPANT_BUILTIN_ENDPOINTS` — legacy builtin-endpoint bitmask.
pub const PARTICIPANT_BUILTIN_ENDPOINTS: u16 = 0x0044;
/// `PID_METATRAFFIC_UNICAST_IPADDRESS` — legacy IPv4 discovery address.
pub const METATRAFFIC_UNICAST_IPADDRESS: u16 = 0x0045;
/// `PID_METATRAFFIC_MULTICAST_PORT` — legacy discovery multicast port.
pub const METATRAFFIC_MULTICAST_PORT: u16 = 0x0046;
/// `PID_DEFAULT_MULTICAST_LOCATOR` — a participant's default user-traffic
/// multicast locator.
pub const DEFAULT_MULTICAST_LOCATOR: u16 = 0x0048;
/// `PID_TRANSPORT_PRIORITY` — the transport-priority QoS policy.
pub const TRANSPORT_PRIORITY: u16 = 0x0049;

/// `PID_PARTICIPANT_GUID` — the participant's GUID.
pub const PARTICIPANT_GUID: u16 = 0x0050;
/// `PID_PARTICIPANT_ENTITYID` — the participant's entity id.
pub const PARTICIPANT_ENTITYID: u16 = 0x0051;
/// `PID_GROUP_GUID` — the publisher's or subscriber's GUID.
pub const GROUP_GUID: u16 = 0x0052;
/// `PID_GROUP_ENTITYID` — the publisher's or subscriber's entity id.
pub const GROUP_ENTITYID: u16 = 0x0053;
/// `PID_BUILTIN_ENDPOINT_SET` — bitmask of the builtin endpoints a
/// participant runs. The value SPDP matching turns on.
pub const BUILTIN_ENDPOINT_SET: u16 = 0x0058;
/// `PID_PROPERTY_LIST` — name/value properties, the vehicle for the ROS 2
/// node-name and namespace annotations.
pub const PROPERTY_LIST: u16 = 0x0059;
/// `PID_ENDPOINT_GUID` — the reader's or writer's GUID in a SEDP sample.
pub const ENDPOINT_GUID: u16 = 0x005a;
/// `PID_TYPE_MAX_SIZE_SERIALIZED` — the largest serialized sample of the
/// topic's type.
pub const TYPE_MAX_SIZE_SERIALIZED: u16 = 0x0060;
/// `PID_ENTITY_NAME` — a human-readable endpoint name.
pub const ENTITY_NAME: u16 = 0x0062;
/// `PID_KEY_HASH` — the 16-octet key hash of a sample. Appears inline on
/// `DATA` submessages, not only in discovery.
pub const KEY_HASH: u16 = 0x0070;
/// `PID_STATUS_INFO` — the dispose/unregister flags of a sample.
pub const STATUS_INFO: u16 = 0x0071;
/// `PID_BUILTIN_ENDPOINT_QOS` — QoS of the builtin endpoints.
pub const BUILTIN_ENDPOINT_QOS: u16 = 0x0077;
/// `PID_DOMAIN_ID` — the DDS domain the participant joined.
pub const DOMAIN_ID: u16 = 0x000f;
/// `PID_DOMAIN_TAG` — the domain tag, an RTPS 2.3 addition.
pub const DOMAIN_TAG: u16 = 0x4014;

/// `PID_DATA_REPRESENTATION` — the data-representation QoS policy: which CDR
/// versions an endpoint accepts. The parameter that decides whether a peer
/// may speak XCDR2 to us.
pub const DATA_REPRESENTATION: u16 = 0x0073;
/// `PID_TYPE_CONSISTENCY_ENFORCEMENT` — the type-consistency QoS policy.
pub const TYPE_CONSISTENCY_ENFORCEMENT: u16 = 0x0074;
/// `PID_TYPE_INFORMATION` — the XTypes `TypeInformation` of the topic type.
pub const TYPE_INFORMATION: u16 = 0x0075;

/// `PID_EXTENDED` — the long-form parameter escape.
///
/// This crate does **not** implement its layout. No C/C++-free in-repo source
/// pins it (blueprint §18), and a fabricated layout would be worse than a
/// clean refusal: [`ParameterList::decode`](super::ParameterList::decode)
/// raises [`CdrError::UnsupportedParameter`](crate::CdrError::UnsupportedParameter)
/// when it sees this id.
pub const EXTENDED: u16 = 0x3f01;

/// `PID_LIST_END` — the alternative list terminator of the extended
/// parameter space. Recognised for the same reason as [`EXTENDED`] and
/// refused the same way.
pub const LIST_END: u16 = 0x3f02;

/// The parameter ids this crate refuses rather than guesses at.
pub const UNSUPPORTED: [u16; 2] = [EXTENDED, LIST_END];

/// The name of a standard parameter id, or `None` when it is not in the
/// table.
///
/// For log messages: an unrecognised discovery parameter should be reported
/// by number, and a recognised one by name.
#[must_use]
pub fn name(id: u16) -> Option<&'static str> {
    // `PID_DOMAIN_TAG` is one of the few standard ids that carries the
    // must-understand flag in its assigned value, so it is matched before the
    // flags are masked off.
    if id == DOMAIN_TAG {
        return Some("PID_DOMAIN_TAG");
    }
    let name = match id & BASE_MASK {
        PAD => "PID_PAD",
        SENTINEL => "PID_SENTINEL",
        PARTICIPANT_LEASE_DURATION => "PID_PARTICIPANT_LEASE_DURATION",
        TIME_BASED_FILTER => "PID_TIME_BASED_FILTER",
        TOPIC_NAME => "PID_TOPIC_NAME",
        OWNERSHIP_STRENGTH => "PID_OWNERSHIP_STRENGTH",
        TYPE_NAME => "PID_TYPE_NAME",
        METATRAFFIC_MULTICAST_IPADDRESS => "PID_METATRAFFIC_MULTICAST_IPADDRESS",
        DEFAULT_UNICAST_IPADDRESS => "PID_DEFAULT_UNICAST_IPADDRESS",
        METATRAFFIC_UNICAST_PORT => "PID_METATRAFFIC_UNICAST_PORT",
        DEFAULT_UNICAST_PORT => "PID_DEFAULT_UNICAST_PORT",
        DOMAIN_ID => "PID_DOMAIN_ID",
        MULTICAST_IPADDRESS => "PID_MULTICAST_IPADDRESS",
        PROTOCOL_VERSION => "PID_PROTOCOL_VERSION",
        VENDORID => "PID_VENDORID",
        RELIABILITY => "PID_RELIABILITY",
        LIVELINESS => "PID_LIVELINESS",
        DURABILITY => "PID_DURABILITY",
        DURABILITY_SERVICE => "PID_DURABILITY_SERVICE",
        OWNERSHIP => "PID_OWNERSHIP",
        PRESENTATION => "PID_PRESENTATION",
        DEADLINE => "PID_DEADLINE",
        DESTINATION_ORDER => "PID_DESTINATION_ORDER",
        LATENCY_BUDGET => "PID_LATENCY_BUDGET",
        PARTITION => "PID_PARTITION",
        LIFESPAN => "PID_LIFESPAN",
        USER_DATA => "PID_USER_DATA",
        GROUP_DATA => "PID_GROUP_DATA",
        TOPIC_DATA => "PID_TOPIC_DATA",
        UNICAST_LOCATOR => "PID_UNICAST_LOCATOR",
        MULTICAST_LOCATOR => "PID_MULTICAST_LOCATOR",
        DEFAULT_UNICAST_LOCATOR => "PID_DEFAULT_UNICAST_LOCATOR",
        METATRAFFIC_UNICAST_LOCATOR => "PID_METATRAFFIC_UNICAST_LOCATOR",
        METATRAFFIC_MULTICAST_LOCATOR => "PID_METATRAFFIC_MULTICAST_LOCATOR",
        PARTICIPANT_MANUAL_LIVELINESS_COUNT => "PID_PARTICIPANT_MANUAL_LIVELINESS_COUNT",
        CONTENT_FILTER_PROPERTY => "PID_CONTENT_FILTER_PROPERTY",
        HISTORY => "PID_HISTORY",
        RESOURCE_LIMITS => "PID_RESOURCE_LIMITS",
        EXPECTS_INLINE_QOS => "PID_EXPECTS_INLINE_QOS",
        PARTICIPANT_BUILTIN_ENDPOINTS => "PID_PARTICIPANT_BUILTIN_ENDPOINTS",
        METATRAFFIC_UNICAST_IPADDRESS => "PID_METATRAFFIC_UNICAST_IPADDRESS",
        METATRAFFIC_MULTICAST_PORT => "PID_METATRAFFIC_MULTICAST_PORT",
        DEFAULT_MULTICAST_LOCATOR => "PID_DEFAULT_MULTICAST_LOCATOR",
        TRANSPORT_PRIORITY => "PID_TRANSPORT_PRIORITY",
        PARTICIPANT_GUID => "PID_PARTICIPANT_GUID",
        PARTICIPANT_ENTITYID => "PID_PARTICIPANT_ENTITYID",
        GROUP_GUID => "PID_GROUP_GUID",
        GROUP_ENTITYID => "PID_GROUP_ENTITYID",
        BUILTIN_ENDPOINT_SET => "PID_BUILTIN_ENDPOINT_SET",
        PROPERTY_LIST => "PID_PROPERTY_LIST",
        ENDPOINT_GUID => "PID_ENDPOINT_GUID",
        TYPE_MAX_SIZE_SERIALIZED => "PID_TYPE_MAX_SIZE_SERIALIZED",
        ENTITY_NAME => "PID_ENTITY_NAME",
        KEY_HASH => "PID_KEY_HASH",
        STATUS_INFO => "PID_STATUS_INFO",
        DATA_REPRESENTATION => "PID_DATA_REPRESENTATION",
        TYPE_CONSISTENCY_ENFORCEMENT => "PID_TYPE_CONSISTENCY_ENFORCEMENT",
        TYPE_INFORMATION => "PID_TYPE_INFORMATION",
        BUILTIN_ENDPOINT_QOS => "PID_BUILTIN_ENDPOINT_QOS",
        EXTENDED => "PID_EXTENDED",
        LIST_END => "PID_LIST_END",
        _ => return None,
    };
    Some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_structural_ids_are_zero_and_one() {
        assert_eq!(PAD, 0x0000);
        assert_eq!(SENTINEL, 0x0001);
    }

    #[test]
    fn flags_occupy_the_top_two_bits() {
        assert_eq!(FLAG_VENDOR_SPECIFIC, 1 << 15);
        assert_eq!(FLAG_MUST_UNDERSTAND, 1 << 14);
        assert_eq!(BASE_MASK, !(FLAG_VENDOR_SPECIFIC | FLAG_MUST_UNDERSTAND));
    }

    #[test]
    fn names_resolve_through_the_flags() {
        assert_eq!(name(TOPIC_NAME), Some("PID_TOPIC_NAME"));
        assert_eq!(
            name(TOPIC_NAME | FLAG_MUST_UNDERSTAND),
            Some("PID_TOPIC_NAME")
        );
        assert_eq!(name(PARTICIPANT_GUID), Some("PID_PARTICIPANT_GUID"));
        // PID_DOMAIN_TAG carries the must-understand flag in its assigned
        // value, so it is recognised before the mask is applied.
        assert_eq!(name(DOMAIN_TAG), Some("PID_DOMAIN_TAG"));
        assert_eq!(name(DOMAIN_ID), Some("PID_DOMAIN_ID"));
        assert_eq!(name(0x3ffe), None);
    }

    #[test]
    fn the_discovery_ids_have_their_specified_values() {
        // The subset SPDP and SEDP samples are built from; each is asserted
        // against the value OMG DDSI-RTPS 2.3 §9.6.2.2 assigns it.
        assert_eq!(PARTICIPANT_LEASE_DURATION, 0x0002);
        assert_eq!(TOPIC_NAME, 0x0005);
        assert_eq!(TYPE_NAME, 0x0007);
        assert_eq!(PROTOCOL_VERSION, 0x0015);
        assert_eq!(VENDORID, 0x0016);
        assert_eq!(RELIABILITY, 0x001a);
        assert_eq!(DURABILITY, 0x001d);
        assert_eq!(USER_DATA, 0x002c);
        assert_eq!(DEFAULT_UNICAST_LOCATOR, 0x0031);
        assert_eq!(METATRAFFIC_UNICAST_LOCATOR, 0x0032);
        assert_eq!(METATRAFFIC_MULTICAST_LOCATOR, 0x0033);
        assert_eq!(PARTICIPANT_GUID, 0x0050);
        assert_eq!(BUILTIN_ENDPOINT_SET, 0x0058);
        assert_eq!(ENDPOINT_GUID, 0x005a);
        assert_eq!(KEY_HASH, 0x0070);
        assert_eq!(STATUS_INFO, 0x0071);
        assert_eq!(DATA_REPRESENTATION, 0x0073);
    }

    #[test]
    fn the_unsupported_set_is_the_extended_escape() {
        assert_eq!(UNSUPPORTED, [0x3f01, 0x3f02]);
    }
}
