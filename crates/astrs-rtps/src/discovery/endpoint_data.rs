//! SEDP endpoint samples: `DiscoveredWriterData` and `DiscoveredReaderData`.
//!
//! Once two participants know each other from SPDP, each announces its
//! *endpoints* on two builtin topics — publications and subscriptions — and
//! the receiving side runs the request-versus-offered rules to decide which
//! pairs to wire up. OMG DDSI-RTPS 2.3 §8.5.4, Tables 9.10 and 9.12.
//!
//! # Shape
//!
//! Both samples share an identity — GUID, owning participant, topic name,
//! type name, and the locators the endpoint prefers — which is
//! [`EndpointIdentity`]. What differs is the QoS: a writer announces
//! `LIFESPAN` and `OWNERSHIP_STRENGTH`, which mean nothing on a reader; a
//! reader announces `TIME_BASED_FILTER` and `EXPECTS_INLINE_QOS`, which mean
//! nothing on a writer. Keeping the two as separate types rather than one
//! with dead fields is what makes [`WriterQos`] and [`ReaderQos`] usable
//! without a "which half is real" comment.
//!
//! # Required parameters
//!
//! Three, on both samples: `PID_ENDPOINT_GUID`, `PID_TOPIC_NAME` and
//! `PID_TYPE_NAME`. An endpoint sample without them cannot be matched against
//! anything, so it is rejected rather than stored with a blank name — a blank
//! name would match another blank name, and two unrelated endpoints would be
//! wired together.
//!
//! Everything else takes the DDS default when absent, unknown parameters are
//! skipped, and each bounded field is checked against
//! [`plist`](crate::discovery::plist)'s ceilings before it is stored.
//!
//! # Locators
//!
//! An endpoint may announce its own locators, or none at all. None means
//! "use the participant's default locators", which is the common case and the
//! one ROS 2 takes: an `rmw` node has one user-traffic socket per
//! participant, not per publisher. [`EndpointIdentity::resolve_locators`]
//! applies that rule.

use core::fmt;

use astrs_cdr::{Encoding, ParameterId, ParameterList, pid};

use crate::behavior::error::{BehaviorError, BehaviorResult};
use crate::discovery::matching::{ReaderQos, WriterQos};
use crate::discovery::plist::{
    MAX_NAME_LEN, decode_locators, decode_octets, decode_one, decode_required,
    decode_required_string,
};
use crate::discovery::qos::ReliabilityQos;
use crate::structure::{DdsDuration, EntityId, Guid, Locator};

/// The `context` a publication sample's errors carry.
pub const PUBLICATION_CONTEXT: &str = "SEDP publication";

/// The `context` a subscription sample's errors carry.
pub const SUBSCRIPTION_CONTEXT: &str = "SEDP subscription";

/// Longest `PID_USER_DATA`, `PID_TOPIC_DATA` or `PID_GROUP_DATA` this decoder
/// accepts.
pub const MAX_ENDPOINT_DATA_LEN: usize = 4_096;

/// `DATA_REPRESENTATION` identifier for XCDR1 — what Humble speaks.
pub const XCDR1_REPRESENTATION: i16 = 0;

/// `DATA_REPRESENTATION` identifier for XCDR2 — what Jazzy adds.
pub const XCDR2_REPRESENTATION: i16 = 2;

/// The parts of an SEDP sample a writer and a reader announce identically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointIdentity {
    /// The endpoint's own GUID. Its prefix names the participant.
    pub guid: Guid,
    /// The participant that owns it, when the sample says so.
    ///
    /// Redundant with `guid.prefix` and sent anyway by every stack, so it is
    /// carried but never trusted over the prefix.
    pub participant_guid: Option<Guid>,
    /// The DDS topic name — for ROS 2, the mangled `rt/…`, `rq/…` or `rr/…`
    /// form.
    pub topic_name: String,
    /// The DDS type name — for ROS 2, `pkg::msg::dds_::Type_`.
    pub type_name: String,
    /// Unicast locators this endpoint prefers, or empty for the
    /// participant's defaults.
    pub unicast: Vec<Locator>,
    /// Multicast locators this endpoint prefers, or empty.
    pub multicast: Vec<Locator>,
    /// Opaque application data attached to the endpoint.
    pub user_data: Vec<u8>,
    /// Opaque application data attached to the topic.
    pub topic_data: Vec<u8>,
    /// Opaque application data attached to the publisher or subscriber.
    pub group_data: Vec<u8>,
    /// The CDR representations the endpoint accepts, when it says.
    ///
    /// Humble omits this or sends `[0]`; Jazzy sends `[0, 2]`.
    pub data_representation: Vec<i16>,
}

impl EndpointIdentity {
    /// A minimal identity: GUID, topic, type, nothing else.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::EmptyName`] when either name is empty, and
    /// [`BehaviorError::NameTooLong`] when either is above
    /// [`MAX_NAME_LEN`].
    pub fn new(
        guid: Guid,
        topic_name: impl Into<String>,
        type_name: impl Into<String>,
    ) -> BehaviorResult<Self> {
        let topic_name = topic_name.into();
        let type_name = type_name.into();
        check_name("topic name", &topic_name)?;
        check_name("type name", &type_name)?;
        Ok(Self {
            guid,
            participant_guid: Some(guid.participant_guid()),
            topic_name,
            type_name,
            unicast: Vec::new(),
            multicast: Vec::new(),
            user_data: Vec::new(),
            topic_data: Vec::new(),
            group_data: Vec::new(),
            data_representation: Vec::new(),
        })
    }

    /// Add a unicast locator this endpoint prefers.
    #[must_use]
    pub fn with_unicast(mut self, locator: Locator) -> Self {
        self.unicast.push(locator);
        self
    }

    /// Add a multicast locator this endpoint prefers.
    #[must_use]
    pub fn with_multicast(mut self, locator: Locator) -> Self {
        self.multicast.push(locator);
        self
    }

    /// Announce the CDR representations the endpoint accepts.
    #[must_use]
    pub fn with_data_representation(mut self, representations: impl Into<Vec<i16>>) -> Self {
        self.data_representation = representations.into();
        self
    }

    /// The endpoint's entity id.
    #[must_use]
    pub const fn entity_id(&self) -> EntityId {
        self.guid.entity_id
    }

    /// The GUID of the participant that owns this endpoint.
    ///
    /// Taken from the endpoint's own GUID prefix, which is authoritative;
    /// `participant_guid` is only a cross-check.
    #[must_use]
    pub const fn owner(&self) -> Guid {
        self.guid.participant_guid()
    }

    /// True when the sample's `PID_PARTICIPANT_GUID`, if present, agrees with
    /// the endpoint GUID's own prefix.
    #[must_use]
    pub fn owner_is_consistent(&self) -> bool {
        match self.participant_guid {
            None => true,
            Some(announced) => announced.prefix == self.guid.prefix,
        }
    }

    /// The locators to send this endpoint's traffic to.
    ///
    /// The endpoint's own, when it announced any; otherwise the
    /// participant's defaults, which is the ROS 2 case.
    #[must_use]
    pub fn resolve_locators(&self, participant_defaults: &[Locator]) -> Vec<Locator> {
        if self.unicast.is_empty() && self.multicast.is_empty() {
            return participant_defaults.to_vec();
        }
        let mut locators = self.unicast.clone();
        locators.extend(self.multicast.iter().copied());
        locators
    }

    /// True when this endpoint accepts XCDR2-encoded samples.
    ///
    /// An endpoint that announces nothing is XCDR1-only, which is the
    /// specification's default and Humble's behaviour.
    #[must_use]
    pub fn accepts_xcdr2(&self) -> bool {
        self.data_representation.contains(&XCDR2_REPRESENTATION)
    }

    /// Write the shared parameters into `list`.
    fn write_common(&self, list: &mut ParameterList<'static>) -> BehaviorResult<()> {
        list.push_value(ParameterId::new(pid::ENDPOINT_GUID), &self.guid)?;
        if let Some(participant) = self.participant_guid {
            list.push_value(ParameterId::new(pid::PARTICIPANT_GUID), &participant)?;
        }
        list.push_value(ParameterId::new(pid::TOPIC_NAME), self.topic_name.as_str())?;
        list.push_value(ParameterId::new(pid::TYPE_NAME), self.type_name.as_str())?;
        for locator in &self.unicast {
            list.push_value(ParameterId::new(pid::UNICAST_LOCATOR), locator)?;
        }
        for locator in &self.multicast {
            list.push_value(ParameterId::new(pid::MULTICAST_LOCATOR), locator)?;
        }
        if !self.user_data.is_empty() {
            list.push_value(ParameterId::new(pid::USER_DATA), &self.user_data)?;
        }
        if !self.topic_data.is_empty() {
            list.push_value(ParameterId::new(pid::TOPIC_DATA), &self.topic_data)?;
        }
        if !self.group_data.is_empty() {
            list.push_value(ParameterId::new(pid::GROUP_DATA), &self.group_data)?;
        }
        if !self.data_representation.is_empty() {
            list.push_value(
                ParameterId::new(pid::DATA_REPRESENTATION),
                &self.data_representation,
            )?;
        }
        Ok(())
    }

    /// Read the shared parameters out of `list`.
    fn read_common(
        list: &ParameterList<'_>,
        encoding: Encoding,
        context: &'static str,
    ) -> BehaviorResult<Self> {
        let guid: Guid = decode_required(list, pid::ENDPOINT_GUID, encoding, context)?;
        let topic_name =
            decode_required_string(list, pid::TOPIC_NAME, encoding, MAX_NAME_LEN, context)?;
        let type_name =
            decode_required_string(list, pid::TYPE_NAME, encoding, MAX_NAME_LEN, context)?;
        if topic_name.is_empty() {
            return Err(BehaviorError::EmptyName {
                field: "topic name",
            });
        }
        if type_name.is_empty() {
            return Err(BehaviorError::EmptyName { field: "type name" });
        }
        Ok(Self {
            guid,
            participant_guid: decode_one(list, pid::PARTICIPANT_GUID, encoding, context)?,
            topic_name,
            type_name,
            unicast: decode_locators(list, pid::UNICAST_LOCATOR, encoding, context)?,
            multicast: decode_locators(list, pid::MULTICAST_LOCATOR, encoding, context)?,
            user_data: decode_octets(
                list,
                pid::USER_DATA,
                encoding,
                MAX_ENDPOINT_DATA_LEN,
                context,
            )?,
            topic_data: decode_octets(
                list,
                pid::TOPIC_DATA,
                encoding,
                MAX_ENDPOINT_DATA_LEN,
                context,
            )?,
            group_data: decode_octets(
                list,
                pid::GROUP_DATA,
                encoding,
                MAX_ENDPOINT_DATA_LEN,
                context,
            )?,
            data_representation: decode_one(list, pid::DATA_REPRESENTATION, encoding, context)?
                .unwrap_or_default(),
        })
    }
}

impl fmt::Display for EndpointIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} on \"{}\" ({})",
            self.guid, self.topic_name, self.type_name
        )
    }
}

/// One publication, as SEDP announces it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredWriterData {
    /// GUID, topic, type and locators.
    pub identity: EndpointIdentity,
    /// The QoS the writer offers.
    pub qos: WriterQos,
    /// The writer's declared upper bound on a serialized sample, when it
    /// declares one.
    pub max_serialized_size: Option<u32>,
}

impl DiscoveredWriterData {
    /// A publication with default QoS.
    ///
    /// # Errors
    ///
    /// As [`EndpointIdentity::new`].
    pub fn new(
        guid: Guid,
        topic_name: impl Into<String>,
        type_name: impl Into<String>,
    ) -> BehaviorResult<Self> {
        Ok(Self {
            identity: EndpointIdentity::new(guid, topic_name, type_name)?,
            qos: WriterQos::default(),
            max_serialized_size: None,
        })
    }

    /// Replace the QoS.
    #[must_use]
    pub const fn with_qos(mut self, qos: WriterQos) -> Self {
        self.qos = qos;
        self
    }

    /// Replace the identity.
    #[must_use]
    pub fn with_identity(mut self, identity: EndpointIdentity) -> Self {
        self.identity = identity;
        self
    }

    /// The writer's GUID.
    #[must_use]
    pub const fn guid(&self) -> Guid {
        self.identity.guid
    }

    /// Serialize into a `PL_CDR_LE` parameter list.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::Cdr`] when a value will not encode.
    pub fn to_parameter_list(&self) -> BehaviorResult<ParameterList<'static>> {
        let mut list = ParameterList::new(Encoding::DISCOVERY);
        self.identity.write_common(&mut list)?;

        list.push_value(ParameterId::new(pid::RELIABILITY), &self.qos.reliability)?;
        list.push_value(ParameterId::new(pid::DURABILITY), &self.qos.durability)?;
        list.push_value(ParameterId::new(pid::HISTORY), &self.qos.history)?;
        list.push_value(ParameterId::new(pid::DEADLINE), &self.qos.deadline)?;
        list.push_value(ParameterId::new(pid::LIFESPAN), &self.qos.lifespan)?;
        list.push_value(ParameterId::new(pid::LIVELINESS), &self.qos.liveliness)?;
        list.push_value(ParameterId::new(pid::OWNERSHIP), &self.qos.ownership)?;
        list.push_value(
            ParameterId::new(pid::OWNERSHIP_STRENGTH),
            &self.qos.ownership_strength,
        )?;
        list.push_value(
            ParameterId::new(pid::DESTINATION_ORDER),
            &self.qos.destination_order,
        )?;
        list.push_value(
            ParameterId::new(pid::LATENCY_BUDGET),
            &self.qos.latency_budget,
        )?;
        list.push_value(ParameterId::new(pid::PRESENTATION), &self.qos.presentation)?;
        list.push_value(
            ParameterId::new(pid::RESOURCE_LIMITS),
            &self.qos.resource_limits,
        )?;
        if let Some(size) = self.max_serialized_size {
            list.push_value(ParameterId::new(pid::TYPE_MAX_SIZE_SERIALIZED), &size)?;
        }
        Ok(list)
    }

    /// Serialize into a `DATA` submessage payload.
    ///
    /// # Errors
    ///
    /// As [`to_parameter_list`](Self::to_parameter_list).
    pub fn to_payload(&self) -> BehaviorResult<crate::messages::SerializedPayload<'static>> {
        let list = self.to_parameter_list()?;
        Ok(crate::messages::SerializedPayload::from_parameter_list(
            &list,
        )?)
    }

    /// Read a publication out of a parameter list.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::MissingParameter`] for the three required ids,
    /// [`BehaviorError::MalformedParameter`] for a value that will not decode,
    /// and [`BehaviorError::WrongEntityKind`] when the announced entity id is
    /// not a writer.
    pub fn from_parameter_list(
        list: &ParameterList<'_>,
        encoding: Encoding,
    ) -> BehaviorResult<Self> {
        let context = PUBLICATION_CONTEXT;
        let identity = EndpointIdentity::read_common(list, encoding, context)?;
        if !identity.guid.entity_id.is_writer() {
            return Err(BehaviorError::WrongEntityKind {
                context,
                entity_id: identity.guid.entity_id,
            });
        }
        let qos = WriterQos {
            reliability: decode_one(list, pid::RELIABILITY, encoding, context)?
                .unwrap_or_else(ReliabilityQos::reliable),
            durability: decode_one(list, pid::DURABILITY, encoding, context)?.unwrap_or_default(),
            history: decode_one(list, pid::HISTORY, encoding, context)?.unwrap_or_default(),
            deadline: decode_one(list, pid::DEADLINE, encoding, context)?.unwrap_or_default(),
            lifespan: decode_one(list, pid::LIFESPAN, encoding, context)?.unwrap_or_default(),
            liveliness: decode_one(list, pid::LIVELINESS, encoding, context)?.unwrap_or_default(),
            ownership: decode_one(list, pid::OWNERSHIP, encoding, context)?.unwrap_or_default(),
            ownership_strength: decode_one(list, pid::OWNERSHIP_STRENGTH, encoding, context)?
                .unwrap_or_default(),
            destination_order: decode_one(list, pid::DESTINATION_ORDER, encoding, context)?
                .unwrap_or_default(),
            latency_budget: decode_one(list, pid::LATENCY_BUDGET, encoding, context)?
                .unwrap_or_default(),
            presentation: decode_one(list, pid::PRESENTATION, encoding, context)?
                .unwrap_or_default(),
            resource_limits: decode_one(list, pid::RESOURCE_LIMITS, encoding, context)?
                .unwrap_or_default(),
        };
        Ok(Self {
            identity,
            qos,
            max_serialized_size: decode_one(
                list,
                pid::TYPE_MAX_SIZE_SERIALIZED,
                encoding,
                context,
            )?,
        })
    }

    /// Read a publication out of a `DATA` payload.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::NotAParameterList`] plus everything
    /// [`from_parameter_list`](Self::from_parameter_list) reports.
    pub fn from_payload(payload: &crate::messages::SerializedPayload<'_>) -> BehaviorResult<Self> {
        let (list, encoding) = parameter_list_of(payload)?;
        Self::from_parameter_list(&list, encoding)
    }
}

impl fmt::Display for DiscoveredWriterData {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "writer {} [{} {}]",
            self.identity, self.qos.reliability.kind, self.qos.durability.kind
        )
    }
}

/// One subscription, as SEDP announces it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredReaderData {
    /// GUID, topic, type and locators.
    pub identity: EndpointIdentity,
    /// The QoS the reader requests.
    pub qos: ReaderQos,
    /// True when the reader wants inline QoS on every sample it is sent.
    pub expects_inline_qos: bool,
}

impl DiscoveredReaderData {
    /// A subscription with default QoS.
    ///
    /// # Errors
    ///
    /// As [`EndpointIdentity::new`].
    pub fn new(
        guid: Guid,
        topic_name: impl Into<String>,
        type_name: impl Into<String>,
    ) -> BehaviorResult<Self> {
        Ok(Self {
            identity: EndpointIdentity::new(guid, topic_name, type_name)?,
            qos: ReaderQos::default(),
            expects_inline_qos: false,
        })
    }

    /// Replace the QoS.
    #[must_use]
    pub const fn with_qos(mut self, qos: ReaderQos) -> Self {
        self.qos = qos;
        self
    }

    /// Replace the identity.
    #[must_use]
    pub fn with_identity(mut self, identity: EndpointIdentity) -> Self {
        self.identity = identity;
        self
    }

    /// The reader's GUID.
    #[must_use]
    pub const fn guid(&self) -> Guid {
        self.identity.guid
    }

    /// Serialize into a `PL_CDR_LE` parameter list.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::Cdr`] when a value will not encode.
    pub fn to_parameter_list(&self) -> BehaviorResult<ParameterList<'static>> {
        let mut list = ParameterList::new(Encoding::DISCOVERY);
        self.identity.write_common(&mut list)?;

        list.push_value(ParameterId::new(pid::RELIABILITY), &self.qos.reliability)?;
        list.push_value(ParameterId::new(pid::DURABILITY), &self.qos.durability)?;
        list.push_value(ParameterId::new(pid::HISTORY), &self.qos.history)?;
        list.push_value(ParameterId::new(pid::DEADLINE), &self.qos.deadline)?;
        list.push_value(ParameterId::new(pid::LIVELINESS), &self.qos.liveliness)?;
        list.push_value(ParameterId::new(pid::OWNERSHIP), &self.qos.ownership)?;
        list.push_value(
            ParameterId::new(pid::DESTINATION_ORDER),
            &self.qos.destination_order,
        )?;
        list.push_value(
            ParameterId::new(pid::LATENCY_BUDGET),
            &self.qos.latency_budget,
        )?;
        list.push_value(ParameterId::new(pid::PRESENTATION), &self.qos.presentation)?;
        list.push_value(
            ParameterId::new(pid::RESOURCE_LIMITS),
            &self.qos.resource_limits,
        )?;
        if !self.qos.time_based_filter.is_zero() {
            list.push_value(
                ParameterId::new(pid::TIME_BASED_FILTER),
                &self.qos.time_based_filter,
            )?;
        }
        if self.expects_inline_qos {
            list.push_value(ParameterId::new(pid::EXPECTS_INLINE_QOS), &true)?;
        }
        Ok(list)
    }

    /// Serialize into a `DATA` submessage payload.
    ///
    /// # Errors
    ///
    /// As [`to_parameter_list`](Self::to_parameter_list).
    pub fn to_payload(&self) -> BehaviorResult<crate::messages::SerializedPayload<'static>> {
        let list = self.to_parameter_list()?;
        Ok(crate::messages::SerializedPayload::from_parameter_list(
            &list,
        )?)
    }

    /// Read a subscription out of a parameter list.
    ///
    /// # Errors
    ///
    /// As [`DiscoveredWriterData::from_parameter_list`], with
    /// [`BehaviorError::WrongEntityKind`] raised when the entity id is not a
    /// reader.
    pub fn from_parameter_list(
        list: &ParameterList<'_>,
        encoding: Encoding,
    ) -> BehaviorResult<Self> {
        let context = SUBSCRIPTION_CONTEXT;
        let identity = EndpointIdentity::read_common(list, encoding, context)?;
        if !identity.guid.entity_id.is_reader() {
            return Err(BehaviorError::WrongEntityKind {
                context,
                entity_id: identity.guid.entity_id,
            });
        }
        let qos = ReaderQos {
            reliability: decode_one(list, pid::RELIABILITY, encoding, context)?
                .unwrap_or_else(ReliabilityQos::best_effort),
            durability: decode_one(list, pid::DURABILITY, encoding, context)?.unwrap_or_default(),
            history: decode_one(list, pid::HISTORY, encoding, context)?.unwrap_or_default(),
            deadline: decode_one(list, pid::DEADLINE, encoding, context)?.unwrap_or_default(),
            liveliness: decode_one(list, pid::LIVELINESS, encoding, context)?.unwrap_or_default(),
            ownership: decode_one(list, pid::OWNERSHIP, encoding, context)?.unwrap_or_default(),
            destination_order: decode_one(list, pid::DESTINATION_ORDER, encoding, context)?
                .unwrap_or_default(),
            latency_budget: decode_one(list, pid::LATENCY_BUDGET, encoding, context)?
                .unwrap_or_default(),
            presentation: decode_one(list, pid::PRESENTATION, encoding, context)?
                .unwrap_or_default(),
            resource_limits: decode_one(list, pid::RESOURCE_LIMITS, encoding, context)?
                .unwrap_or_default(),
            time_based_filter: decode_one(list, pid::TIME_BASED_FILTER, encoding, context)?
                .unwrap_or(DdsDuration::ZERO),
        };
        Ok(Self {
            identity,
            qos,
            expects_inline_qos: decode_one(list, pid::EXPECTS_INLINE_QOS, encoding, context)?
                .unwrap_or(false),
        })
    }

    /// Read a subscription out of a `DATA` payload.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::NotAParameterList`] plus everything
    /// [`from_parameter_list`](Self::from_parameter_list) reports.
    pub fn from_payload(payload: &crate::messages::SerializedPayload<'_>) -> BehaviorResult<Self> {
        let (list, encoding) = parameter_list_of(payload)?;
        Self::from_parameter_list(&list, encoding)
    }
}

impl fmt::Display for DiscoveredReaderData {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "reader {} [{} {}]",
            self.identity, self.qos.reliability.kind, self.qos.durability.kind
        )
    }
}

/// Pull the parameter list out of a discovery payload, refusing anything that
/// is not `PL_CDR`.
fn parameter_list_of<'a>(
    payload: &'a crate::messages::SerializedPayload<'a>,
) -> BehaviorResult<(ParameterList<'a>, Encoding)> {
    if !payload.is_parameter_list() {
        let identifier = payload
            .encapsulation()
            .map_or(0xffff, astrs_cdr::EncapsulationKind::identifier);
        return Err(BehaviorError::NotAParameterList { identifier });
    }
    Ok(payload.parameter_list()?)
}

/// Reject an empty or over-long topic or type name.
fn check_name(field: &'static str, value: &str) -> BehaviorResult<()> {
    if value.is_empty() {
        return Err(BehaviorError::EmptyName { field });
    }
    if value.len() > MAX_NAME_LEN {
        return Err(BehaviorError::NameTooLong {
            field,
            len: value.len(),
            limit: MAX_NAME_LEN,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::discovery::qos::{DurabilityKind, ReliabilityKind};
    use crate::structure::{EntityKind, GuidPrefix, VendorId};
    use std::net::Ipv4Addr;

    const TOPIC: &str = "rt/chatter";
    const TYPE: &str = "std_msgs::msg::dds_::String_";

    fn prefix() -> GuidPrefix {
        GuidPrefix::vendor_scoped(VendorId::ASTRS, [3; 10])
    }

    fn writer_guid(counter: u32) -> Guid {
        Guid::new(
            prefix(),
            EntityId::user_defined(counter, EntityKind::USER_WRITER_NO_KEY),
        )
    }

    fn reader_guid(counter: u32) -> Guid {
        Guid::new(
            prefix(),
            EntityId::user_defined(counter, EntityKind::USER_READER_NO_KEY),
        )
    }

    fn publication() -> DiscoveredWriterData {
        DiscoveredWriterData::new(writer_guid(1), TOPIC, TYPE)
            .unwrap()
            .with_qos(WriterQos::latched(7))
            .with_identity(
                EndpointIdentity::new(writer_guid(1), TOPIC, TYPE)
                    .unwrap()
                    .with_unicast(Locator::udpv4(Ipv4Addr::LOCALHOST, 45_010))
                    .with_data_representation(vec![XCDR1_REPRESENTATION, XCDR2_REPRESENTATION]),
            )
    }

    fn subscription() -> DiscoveredReaderData {
        DiscoveredReaderData::new(reader_guid(1), TOPIC, TYPE)
            .unwrap()
            .with_qos(ReaderQos::latched(7))
    }

    #[test]
    fn a_publication_round_trips_through_its_payload() {
        let original = publication();
        let payload = original.to_payload().unwrap();
        let decoded = DiscoveredWriterData::from_payload(&payload).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn a_subscription_round_trips_through_its_payload() {
        let original = subscription();
        let payload = original.to_payload().unwrap();
        let decoded = DiscoveredReaderData::from_payload(&payload).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn the_qos_survives_intact() {
        let decoded =
            DiscoveredWriterData::from_payload(&publication().to_payload().unwrap()).unwrap();
        assert_eq!(decoded.qos.reliability.kind, ReliabilityKind::Reliable);
        assert_eq!(decoded.qos.durability.kind, DurabilityKind::TransientLocal);
        assert_eq!(decoded.qos.history.depth, 7);
    }

    #[test]
    fn a_publication_with_a_reader_entity_id_is_refused() {
        let mut sample = publication();
        sample.identity.guid = reader_guid(2);
        let payload = sample.to_payload().unwrap();
        let error = DiscoveredWriterData::from_payload(&payload).expect_err("must reject");
        assert!(matches!(error, BehaviorError::WrongEntityKind { .. }));
    }

    #[test]
    fn a_subscription_with_a_writer_entity_id_is_refused() {
        let mut sample = subscription();
        sample.identity.guid = writer_guid(2);
        let payload = sample.to_payload().unwrap();
        let error = DiscoveredReaderData::from_payload(&payload).expect_err("must reject");
        assert!(matches!(error, BehaviorError::WrongEntityKind { .. }));
    }

    #[test]
    fn an_empty_topic_name_is_refused_at_construction() {
        let error = DiscoveredWriterData::new(writer_guid(1), "", TYPE).expect_err("must reject");
        assert_eq!(
            error,
            BehaviorError::EmptyName {
                field: "topic name"
            }
        );
    }

    #[test]
    fn an_over_long_type_name_is_refused_at_construction() {
        let long = "z".repeat(MAX_NAME_LEN + 1);
        let error =
            DiscoveredReaderData::new(reader_guid(1), TOPIC, long).expect_err("must reject");
        assert!(matches!(
            error,
            BehaviorError::NameTooLong {
                field: "type name",
                ..
            }
        ));
    }

    #[test]
    fn the_three_required_parameters_are_required() {
        for missing in [pid::ENDPOINT_GUID, pid::TOPIC_NAME, pid::TYPE_NAME] {
            let full = publication().to_parameter_list().unwrap();
            let mut trimmed = ParameterList::new(Encoding::DISCOVERY);
            for parameter in full.iter() {
                if parameter.id.base() != missing {
                    trimmed.push(parameter.clone());
                }
            }
            let error = DiscoveredWriterData::from_parameter_list(&trimmed, Encoding::DISCOVERY)
                .expect_err("must reject");
            assert_eq!(
                error,
                BehaviorError::MissingParameter {
                    context: PUBLICATION_CONTEXT,
                    pid: missing,
                },
                "removing 0x{missing:04x} must be fatal"
            );
        }
    }

    #[test]
    fn a_sample_with_no_qos_parameters_takes_the_defaults() {
        let mut list = ParameterList::new(Encoding::DISCOVERY);
        list.push_value(ParameterId::new(pid::ENDPOINT_GUID), &writer_guid(4))
            .unwrap();
        list.push_value(ParameterId::new(pid::TOPIC_NAME), TOPIC)
            .unwrap();
        list.push_value(ParameterId::new(pid::TYPE_NAME), TYPE)
            .unwrap();

        let writer = DiscoveredWriterData::from_parameter_list(&list, Encoding::DISCOVERY).unwrap();
        assert_eq!(writer.qos, WriterQos::default());
        assert!(writer.qos.is_reliable(), "a writer defaults to RELIABLE");

        let mut reader_list = ParameterList::new(Encoding::DISCOVERY);
        reader_list
            .push_value(ParameterId::new(pid::ENDPOINT_GUID), &reader_guid(4))
            .unwrap();
        reader_list
            .push_value(ParameterId::new(pid::TOPIC_NAME), TOPIC)
            .unwrap();
        reader_list
            .push_value(ParameterId::new(pid::TYPE_NAME), TYPE)
            .unwrap();
        let reader =
            DiscoveredReaderData::from_parameter_list(&reader_list, Encoding::DISCOVERY).unwrap();
        assert_eq!(reader.qos, ReaderQos::default());
        assert!(
            !reader.qos.is_reliable(),
            "a reader defaults to BEST_EFFORT"
        );
    }

    #[test]
    fn locators_fall_back_to_the_participant_defaults() {
        let bare = EndpointIdentity::new(reader_guid(5), TOPIC, TYPE).unwrap();
        let defaults = [Locator::udpv4(Ipv4Addr::LOCALHOST, 7411)];
        assert_eq!(bare.resolve_locators(&defaults), defaults.to_vec());

        let specific = bare
            .clone()
            .with_unicast(Locator::udpv4(Ipv4Addr::LOCALHOST, 9999));
        assert_eq!(
            specific.resolve_locators(&defaults),
            vec![Locator::udpv4(Ipv4Addr::LOCALHOST, 9999)]
        );
    }

    #[test]
    fn the_owner_comes_from_the_prefix_not_the_announcement() {
        let mut identity = EndpointIdentity::new(writer_guid(6), TOPIC, TYPE).unwrap();
        assert!(identity.owner_is_consistent());
        assert_eq!(identity.owner(), writer_guid(6).participant_guid());

        identity.participant_guid = Some(Guid::new(
            GuidPrefix::vendor_scoped(VendorId::ASTRS, [9; 10]),
            crate::structure::ENTITYID_PARTICIPANT,
        ));
        assert!(
            !identity.owner_is_consistent(),
            "a lying PID_PARTICIPANT_GUID must be detectable"
        );
        assert_eq!(
            identity.owner(),
            writer_guid(6).participant_guid(),
            "the prefix still wins"
        );
    }

    #[test]
    fn data_representation_decides_xcdr2() {
        let humble = EndpointIdentity::new(writer_guid(7), TOPIC, TYPE).unwrap();
        assert!(!humble.accepts_xcdr2(), "silence means XCDR1");

        let jazzy = humble
            .clone()
            .with_data_representation(vec![XCDR1_REPRESENTATION, XCDR2_REPRESENTATION]);
        assert!(jazzy.accepts_xcdr2());
    }

    #[test]
    fn the_time_based_filter_only_appears_when_set() {
        let plain = subscription();
        let list = plain.to_parameter_list().unwrap();
        assert!(list.get_by_base(pid::TIME_BASED_FILTER).is_none());

        let mut filtered = subscription();
        filtered.qos.time_based_filter = DdsDuration::from_millis(50);
        let list = filtered.to_parameter_list().unwrap();
        assert!(list.get_by_base(pid::TIME_BASED_FILTER).is_some());

        let decoded = DiscoveredReaderData::from_payload(&filtered.to_payload().unwrap()).unwrap();
        assert_eq!(decoded.qos.time_based_filter, DdsDuration::from_millis(50));
    }

    #[test]
    fn expects_inline_qos_round_trips() {
        let mut sample = subscription();
        sample.expects_inline_qos = true;
        let decoded = DiscoveredReaderData::from_payload(&sample.to_payload().unwrap()).unwrap();
        assert!(decoded.expects_inline_qos);
    }

    #[test]
    fn the_max_serialized_size_round_trips() {
        let mut sample = publication();
        sample.max_serialized_size = Some(1_048_576);
        let decoded = DiscoveredWriterData::from_payload(&sample.to_payload().unwrap()).unwrap();
        assert_eq!(decoded.max_serialized_size, Some(1_048_576));
    }

    #[test]
    fn a_payload_that_is_not_a_parameter_list_is_named_as_such() {
        let payload = crate::messages::SerializedPayload::from_cdr(&1_u8).unwrap();
        let error = DiscoveredWriterData::from_payload(&payload).expect_err("must reject");
        assert!(matches!(error, BehaviorError::NotAParameterList { .. }));
    }

    #[test]
    fn opaque_data_round_trips() {
        let mut sample = publication();
        sample.identity.user_data = b"user".to_vec();
        sample.identity.topic_data = b"topic".to_vec();
        sample.identity.group_data = b"group".to_vec();
        let decoded = DiscoveredWriterData::from_payload(&sample.to_payload().unwrap()).unwrap();
        assert_eq!(decoded.identity.user_data, b"user");
        assert_eq!(decoded.identity.topic_data, b"topic");
        assert_eq!(decoded.identity.group_data, b"group");
    }

    #[test]
    fn display_names_topic_type_and_qos() {
        let rendered = publication().to_string();
        assert!(rendered.contains(TOPIC), "{rendered}");
        assert!(rendered.contains("RELIABLE"), "{rendered}");
        assert!(rendered.contains("TRANSIENT_LOCAL"), "{rendered}");
    }
}
