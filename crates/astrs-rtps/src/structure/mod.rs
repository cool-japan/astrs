//! RTPS value types: the vocabulary every submessage is written in.
//!
//! This module is the PSM half of OMG DDSI-RTPS 2.3 §9.3 and §9.4.2 — the
//! *types*, not the entities. Nothing here holds protocol state, opens a
//! socket, or knows what a writer is; every type is a plain value with a
//! fixed wire layout, a `Display` a log line can use, and the predicates the
//! §8.3.7 validity clauses are written against.
//!
//! | Module | Types | Specification |
//! |---|---|---|
//! | [`protocol`] | [`PROTOCOL_ID`], [`ProtocolVersion`], [`VendorId`] | §9.3.1, §9.4.2.2 |
//! | [`guid`] | [`GuidPrefix`], [`EntityId`], [`EntityKind`], [`Guid`], the `ENTITYID_*` set | §8.2.4, §9.3.1 |
//! | [`sequence`] | [`SequenceNumber`], [`SequenceNumberSet`] | §9.4.2.5, §9.4.2.6 |
//! | [`fragment`] | [`FragmentNumber`], [`FragmentNumberSet`] | §9.4.2.7, §9.4.2.8 |
//! | [`locator`] | [`Locator`], [`LocatorKind`], [`LocatorList`] | §9.4.2.11 |
//! | [`time`] | [`Time`], [`Duration`], [`DdsDuration`] | §9.4.2.9 |
//! | [`port`] | the domain/participant port mapping | §9.6.1.1 |
//!
//! # Byte order
//!
//! Two rules, and everything follows from them.
//!
//! 1. **Octet sequences have no byte order.** A [`GuidPrefix`], an
//!    [`EntityId`], a [`VendorId`], a [`ProtocolVersion`], the sixteen
//!    address octets of a [`Locator`] — all are written in declaration order
//!    whatever the submessage's `E` flag says.
//! 2. **Everything else follows the stream.** A [`SequenceNumber`]'s two
//!    halves, a [`Locator`]'s `kind` and `port`, a [`Time`]'s two fields, and
//!    every `numBits` and bitmap word take the byte order of the submessage
//!    they appear in.
//!
//! Both rules are exercised by the round-trip tests in each module, which run
//! every type through `CDR_LE` and `CDR_BE` and assert the octets.
//!
//! # CDR integration
//!
//! Every fixed-size type here implements `astrs-cdr`'s
//! [`CdrSerialize`](astrs_cdr::CdrSerialize) and
//! [`CdrDeserialize`](astrs_cdr::CdrDeserialize), which is what lets the
//! discovery half build an SPDP or SEDP sample straight out of a
//! [`ParameterList`](astrs_cdr::ParameterList):
//!
//! ```
//! use astrs_cdr::{Encoding, ParameterList, ParameterId, pid};
//! use astrs_rtps::structure::{Duration, ENTITYID_PARTICIPANT, Guid, GuidPrefix, VendorId};
//!
//! let mut announcement = ParameterList::new(Encoding::DISCOVERY);
//! let guid = Guid::new(
//!     GuidPrefix::vendor_scoped(VendorId::ASTRS, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]),
//!     ENTITYID_PARTICIPANT,
//! );
//! announcement.push_value(ParameterId::new(pid::PARTICIPANT_GUID), &guid)?;
//! announcement.push_value(
//!     ParameterId::new(pid::PARTICIPANT_LEASE_DURATION),
//!     &Duration::from_secs(30),
//! )?;
//! assert_eq!(announcement.len(), 2);
//! # Ok::<(), astrs_cdr::CdrError>(())
//! ```
//!
//! The two variable-length types — [`SequenceNumberSet`],
//! [`FragmentNumberSet`] — and [`LocatorList`] instead expose `read` and
//! `write` methods that return [`RtpsResult`](crate::RtpsResult), because
//! their failures (`numBits > 256`, a hostile locator count) are RTPS
//! validity violations rather than CDR faults.

pub mod fragment;
pub mod guid;
pub mod locator;
pub mod port;
pub mod protocol;
pub mod sequence;
pub mod time;

pub use fragment::{
    FRAGMENT_NUMBER_LEN, FRAGMENT_SET_PREFIX_LEN, FragmentNumber, FragmentNumberSet,
    FragmentNumberSetIter,
};
pub use guid::{
    ENTITY_ID_LEN, ENTITYID_P2P_BUILTIN_PARTICIPANT_MESSAGE_READER,
    ENTITYID_P2P_BUILTIN_PARTICIPANT_MESSAGE_WRITER, ENTITYID_PARTICIPANT,
    ENTITYID_SEDP_BUILTIN_PUBLICATIONS_READER, ENTITYID_SEDP_BUILTIN_PUBLICATIONS_WRITER,
    ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_READER, ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_WRITER,
    ENTITYID_SEDP_BUILTIN_TOPIC_READER, ENTITYID_SEDP_BUILTIN_TOPIC_WRITER,
    ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER, ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER,
    ENTITYID_UNKNOWN, EntityCategory, EntityId, EntityKind, EntityRole, GUID_LEN, GUID_PREFIX_LEN,
    GUID_PREFIX_UNIQUE_LEN, Guid, GuidPrefix, WELL_KNOWN,
};
pub use locator::{
    LOCATOR_ADDRESS_INVALID, LOCATOR_ADDRESS_LEN, LOCATOR_LEN, LOCATOR_PORT_INVALID, Locator,
    LocatorKind, LocatorList, MAX_LOCATORS,
};
pub use protocol::{PROTOCOL_ID, PROTOCOL_VERSION_LEN, ProtocolVersion, VENDOR_ID_LEN, VendorId};
pub use sequence::{
    MAX_SET_BITS, MAX_SET_WORDS, SEQUENCE_NUMBER_LEN, SET_PREFIX_LEN, SequenceNumber,
    SequenceNumberSet, SequenceNumberSetIter,
};
pub use time::{DdsDuration, Duration, TIME_LEN, Time};
