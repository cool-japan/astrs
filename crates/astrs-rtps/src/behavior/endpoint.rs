//! The vocabulary writers and readers share: outbound datagrams, delivered
//! samples, and entity-id allocation.
//!
//! # Why writers and readers do no I/O
//!
//! Nothing in [`writer`](crate::behavior::writer) or
//! [`reader`](crate::behavior::reader) touches a socket. A writer's `write`
//! returns [`Outbound`] values — "these octets, to these locators" — and the
//! participant is what puts them on the wire. A reader takes a decoded
//! submessage in and returns a [`Sample`] out.
//!
//! That is not layering for its own sake. It means the entire reliability
//! protocol — heartbeat cadence, ACKNACK bookkeeping, GAP generation,
//! retransmission, fragmentation — is synchronous, deterministic, and
//! testable with no runtime, no ports and no timing. The parts that *are*
//! asynchronous, the socket and the clock, are exactly two, and they live in
//! [`participant`](crate::behavior::participant).
//!
//! # Entity ids
//!
//! §9.3.1.2 splits the four-octet `entityId` into a three-octet key and a
//! one-octet kind. A participant hands out keys from a counter and picks the
//! kind from what the endpoint is, so a user writer and a user reader created
//! in that order get `00 00 01 03` and `00 00 02 04`. Builtin entity ids are
//! fixed by the specification and never come from the counter.

use std::time::Instant;

use crate::behavior::cache::{CacheChange, ChangeKind, InstanceHandle};
use crate::behavior::error::{BehaviorError, BehaviorResult};
use crate::structure::{EntityId, EntityKind, Guid, Locator, SequenceNumber, Time};

/// The largest entity key a three-octet field can hold.
pub const MAX_ENTITY_KEY: u32 = 0x00ff_ffff;

/// An encoded RTPS message and the locators it is for.
///
/// The unit of work a participant's send path consumes. Bundling the octets
/// with their destinations means a writer can decide "this GAP goes only to
/// the reader that asked" without the participant having to reconstruct the
/// reasoning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outbound {
    /// Where to send it. Locators this transport cannot use are skipped by
    /// the sender, not filtered here, so a log can say what was dropped.
    pub locators: Vec<Locator>,
    /// The complete datagram, RTPS header included.
    pub datagram: Vec<u8>,
}

impl Outbound {
    /// Pair octets with destinations.
    #[must_use]
    pub fn new(locators: Vec<Locator>, datagram: Vec<u8>) -> Self {
        Self { locators, datagram }
    }

    /// Octets in the datagram.
    #[must_use]
    pub fn len(&self) -> usize {
        self.datagram.len()
    }

    /// True when there is nothing to send, or nowhere to send it.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.datagram.is_empty() || self.locators.is_empty()
    }

    /// True when at least one destination is addressable over UDP.
    #[must_use]
    pub fn is_deliverable(&self) -> bool {
        self.locators
            .iter()
            .any(|locator| locator.socket_addr().is_ok())
    }
}

/// A sample as a reader delivers it to the application.
///
/// Everything DDS's `SampleInfo` carries that RTPS itself can know: who wrote
/// it, which sequence number it was, when the writer said it was written and
/// when this host received it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    /// The writer that produced it.
    pub writer: Guid,
    /// The writer's sequence number.
    pub sequence_number: SequenceNumber,
    /// The serialized payload, encapsulation header included.
    pub payload: Vec<u8>,
    /// The `INFO_TS` the writer sent, when it sent one.
    pub source_timestamp: Option<Time>,
    /// When this reader accepted it.
    pub received_at: Instant,
    /// Alive, disposed or unregistered.
    pub kind: ChangeKind,
    /// Which instance of the topic it belongs to.
    pub instance: InstanceHandle,
}

impl Sample {
    /// Build a sample from a reader's cache change.
    #[must_use]
    pub fn from_change(writer: Guid, change: CacheChange) -> Self {
        Self {
            writer,
            sequence_number: change.sequence_number,
            payload: change.payload,
            source_timestamp: change.source_timestamp,
            received_at: change.written_at,
            kind: change.kind,
            instance: change.instance,
        }
    }

    /// Octets in the payload.
    #[must_use]
    pub fn len(&self) -> usize {
        self.payload.len()
    }

    /// True when the payload is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.payload.is_empty()
    }

    /// True when the sample carries data rather than a lifecycle change.
    #[must_use]
    pub const fn is_alive(&self) -> bool {
        self.kind.carries_data()
    }

    /// The payload as a slice.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.payload
    }

    /// Decode the payload as a CDR value.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::Cdr`] when the octets are not a well-formed `T`.
    pub fn decode<'de, T: astrs_cdr::CdrDeserialize<'de>>(&'de self) -> BehaviorResult<T> {
        Ok(astrs_cdr::from_bytes(&self.payload)?)
    }
}

/// Hands out `entityId`s for the endpoints a participant creates.
///
/// One counter for the whole participant, as §9.3.1.2 intends: the key is
/// unique within the participant, and the kind octet says what the endpoint
/// is. Builtin ids never come from here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EntityIdAllocator {
    next_key: u32,
}

impl EntityIdAllocator {
    /// An allocator whose first key is 1.
    ///
    /// Key 0 is skipped: `00 00 00 00` is `ENTITYID_UNKNOWN`, and an endpoint
    /// whose id is "unknown" would be addressed by every `DATA` that means
    /// "everybody".
    #[must_use]
    pub const fn new() -> Self {
        Self { next_key: 1 }
    }

    /// The key the next allocation will use.
    #[must_use]
    pub const fn peek(self) -> u32 {
        self.next_key
    }

    /// Allocate an entity id of `kind`.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::EntityKeysExhausted`] once the three-octet key space
    /// is used up.
    pub fn allocate(&mut self, kind: EntityKind) -> BehaviorResult<EntityId> {
        if self.next_key > MAX_ENTITY_KEY {
            return Err(BehaviorError::EntityKeysExhausted { kind: kind.raw() });
        }
        let id = EntityId::user_defined(self.next_key, kind);
        self.next_key = self.next_key.saturating_add(1);
        Ok(id)
    }

    /// Allocate a keyless user writer.
    ///
    /// # Errors
    ///
    /// As [`allocate`](Self::allocate).
    pub fn allocate_writer(&mut self) -> BehaviorResult<EntityId> {
        self.allocate(EntityKind::USER_WRITER_NO_KEY)
    }

    /// Allocate a keyless user reader.
    ///
    /// # Errors
    ///
    /// As [`allocate`](Self::allocate).
    pub fn allocate_reader(&mut self) -> BehaviorResult<EntityId> {
        self.allocate(EntityKind::USER_READER_NO_KEY)
    }
}

/// The two names that decide whether two endpoints belong together.
///
/// DDS matches on both, exactly. Two endpoints on `rt/chatter` carrying
/// different types are as unrelated as two endpoints on different topics.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TopicKey {
    /// The DDS topic name.
    pub topic_name: String,
    /// The DDS type name.
    pub type_name: String,
}

impl TopicKey {
    /// Name a topic, checking both strings.
    ///
    /// # Errors
    ///
    /// [`BehaviorError::EmptyName`] or [`BehaviorError::NameTooLong`].
    pub fn new(
        topic_name: impl Into<String>,
        type_name: impl Into<String>,
    ) -> BehaviorResult<Self> {
        let topic_name = topic_name.into();
        let type_name = type_name.into();
        check("topic name", &topic_name)?;
        check("type name", &type_name)?;
        Ok(Self {
            topic_name,
            type_name,
        })
    }

    /// True when `other` names the same topic and type.
    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        self == other
    }
}

/// Reject an empty or over-long name.
fn check(field: &'static str, value: &str) -> BehaviorResult<()> {
    if value.is_empty() {
        return Err(BehaviorError::EmptyName { field });
    }
    if value.len() > crate::discovery::plist::MAX_NAME_LEN {
        return Err(BehaviorError::NameTooLong {
            field,
            len: value.len(),
            limit: crate::discovery::plist::MAX_NAME_LEN,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::structure::{GuidPrefix, VendorId};
    use std::net::Ipv4Addr;

    fn guid() -> Guid {
        Guid::new(
            GuidPrefix::vendor_scoped(VendorId::ASTRS, [1; 10]),
            EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY),
        )
    }

    #[test]
    fn the_allocator_skips_the_unknown_key() {
        let mut allocator = EntityIdAllocator::new();
        assert_eq!(allocator.peek(), 1);
        let first = allocator.allocate_writer().unwrap();
        assert_eq!(first.key_value(), 1);
        assert_ne!(first, EntityId::UNKNOWN);
    }

    #[test]
    fn writers_and_readers_share_one_counter() {
        let mut allocator = EntityIdAllocator::new();
        let writer = allocator.allocate_writer().unwrap();
        let reader = allocator.allocate_reader().unwrap();
        assert_eq!(writer.to_octets(), [0, 0, 1, 0x03]);
        assert_eq!(reader.to_octets(), [0, 0, 2, 0x04]);
        assert!(writer.is_writer());
        assert!(reader.is_reader());
        assert!(!writer.is_builtin());
    }

    #[test]
    fn the_key_space_is_finite_and_says_so() {
        let mut allocator = EntityIdAllocator {
            next_key: MAX_ENTITY_KEY,
        };
        assert!(
            allocator.allocate_writer().is_ok(),
            "the last key is usable"
        );
        let error = allocator
            .allocate_writer()
            .expect_err("and then it is gone");
        assert!(matches!(error, BehaviorError::EntityKeysExhausted { .. }));
    }

    #[test]
    fn an_outbound_with_no_locators_is_empty() {
        let nowhere = Outbound::new(Vec::new(), vec![1, 2, 3]);
        assert!(nowhere.is_empty());
        assert!(!nowhere.is_deliverable());
        assert_eq!(nowhere.len(), 3);
    }

    #[test]
    fn an_outbound_with_only_unusable_locators_is_not_deliverable() {
        let bad = Outbound::new(vec![Locator::INVALID], vec![1]);
        assert!(!bad.is_empty(), "there are octets and a locator");
        assert!(!bad.is_deliverable(), "but none of them can be sent to");

        let good = Outbound::new(
            vec![Locator::INVALID, Locator::udpv4(Ipv4Addr::LOCALHOST, 7400)],
            vec![1],
        );
        assert!(good.is_deliverable());
    }

    #[test]
    fn a_sample_carries_everything_the_change_had() {
        let change = CacheChange::new(SequenceNumber::new(9), vec![1, 2, 3])
            .with_source_timestamp(Time::new(17, 0))
            .with_kind(ChangeKind::NotAliveDisposed)
            .with_instance(InstanceHandle::new([4; 16]));
        let sample = Sample::from_change(guid(), change);
        assert_eq!(sample.writer, guid());
        assert_eq!(sample.sequence_number, SequenceNumber::new(9));
        assert_eq!(sample.as_slice(), &[1, 2, 3]);
        assert_eq!(sample.source_timestamp, Some(Time::new(17, 0)));
        assert!(!sample.is_alive());
        assert_eq!(sample.len(), 3);
        assert!(!sample.is_empty());
        assert_eq!(sample.instance, InstanceHandle::new([4; 16]));
    }

    #[test]
    fn a_sample_decodes_its_payload() {
        let octets = astrs_cdr::to_vec(&1_234_i32, astrs_cdr::Encoding::ROS2).unwrap();
        let sample = Sample::from_change(guid(), CacheChange::new(SequenceNumber::FIRST, octets));
        assert_eq!(sample.decode::<i32>().unwrap(), 1_234);
    }

    #[test]
    fn a_sample_reports_a_bad_payload_as_a_cdr_error() {
        let sample = Sample::from_change(
            guid(),
            CacheChange::new(SequenceNumber::FIRST, vec![0_u8; 2]),
        );
        assert!(matches!(sample.decode::<i32>(), Err(BehaviorError::Cdr(_))));
    }

    #[test]
    fn a_topic_key_needs_both_names() {
        assert_eq!(
            TopicKey::new("", "T").expect_err("empty topic"),
            BehaviorError::EmptyName {
                field: "topic name"
            }
        );
        assert_eq!(
            TopicKey::new("rt/x", "").expect_err("empty type"),
            BehaviorError::EmptyName { field: "type name" }
        );
        let long = "z".repeat(crate::discovery::plist::MAX_NAME_LEN + 1);
        assert!(matches!(
            TopicKey::new("rt/x", long).expect_err("long type"),
            BehaviorError::NameTooLong { .. }
        ));
    }

    #[test]
    fn topics_match_on_both_names() {
        let left = TopicKey::new("rt/chatter", "std_msgs::msg::dds_::String_").unwrap();
        let same = TopicKey::new("rt/chatter", "std_msgs::msg::dds_::String_").unwrap();
        let other_type = TopicKey::new("rt/chatter", "std_msgs::msg::dds_::Int32_").unwrap();
        let other_topic = TopicKey::new("rt/other", "std_msgs::msg::dds_::String_").unwrap();
        assert!(left.matches(&same));
        assert!(!left.matches(&other_type));
        assert!(!left.matches(&other_topic));
    }
}
