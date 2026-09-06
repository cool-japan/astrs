//! Globally unique identifiers: [`GuidPrefix`], [`EntityId`] and the
//! [`Guid`] that pairs them.
//!
//! Every RTPS entity — participant, writer, reader, group — is named by a
//! sixteen-octet GUID that splits into a twelve-octet prefix identifying the
//! *participant* and a four-octet entity id identifying an entity *within*
//! that participant (OMG DDSI-RTPS 2.3 §8.2.4). The split is what lets a
//! message header carry the prefix once and every submessage carry only the
//! four-octet halves:
//!
//! ```text
//!  Header                                  Submessage
//! +-----------------------+               +-----------+-----------+
//! |     guidPrefix (12)   |    +          | readerId  | writerId  |
//! +-----------------------+               +-----------+-----------+
//!             \_______________ Guid ________________/
//! ```
//!
//! None of these types has a byte order: they are octet sequences, laid down
//! in declaration order on a big-endian machine and on a little-endian one
//! alike (§9.3.1).
//!
//! # Well-known entity ids
//!
//! The built-in entities have fixed ids so that two participants can address
//! each other's discovery endpoints before they have discovered anything.
//! They are all in this module as `ENTITYID_*` constants, and
//! [`EntityId::well_known_name`] maps an id back to its specification name
//! for logging.
//!
//! ```
//! use astrs_rtps::structure::{ENTITYID_PARTICIPANT, EntityId, Guid, GuidPrefix};
//!
//! let participant = Guid::new(GuidPrefix::new([1; 12]), ENTITYID_PARTICIPANT);
//! assert!(participant.entity_id.is_builtin());
//! assert_eq!(
//!     ENTITYID_PARTICIPANT.well_known_name(),
//!     Some("ENTITYID_PARTICIPANT"),
//! );
//! assert_eq!(EntityId::from_octets([0, 0, 1, 0xc1]), ENTITYID_PARTICIPANT);
//! ```

use core::fmt;

use astrs_cdr::{CdrDeserialize, CdrReader, CdrResult, CdrSerialize, CdrType, CdrWriter};

use crate::structure::protocol::VendorId;

/// Octets a [`GuidPrefix`] occupies on the wire.
pub const GUID_PREFIX_LEN: usize = 12;

/// Octets an [`EntityId`] occupies on the wire.
pub const ENTITY_ID_LEN: usize = 4;

/// Octets a [`Guid`] occupies on the wire.
pub const GUID_LEN: usize = GUID_PREFIX_LEN + ENTITY_ID_LEN;

/// Octets of a [`GuidPrefix`] a vendor may choose freely.
///
/// The first two are the vendor id (§9.3.1.5), leaving ten.
pub const GUID_PREFIX_UNIQUE_LEN: usize = GUID_PREFIX_LEN - 2;

// ---------------------------------------------------------------------------
// GuidPrefix
// ---------------------------------------------------------------------------

/// The twelve octets that identify a participant (§9.3.1.5).
///
/// RTPS reserves the first two for the participant's vendor id and leaves the
/// remaining ten to the vendor, which is what
/// [`GuidPrefix::vendor_scoped`] builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct GuidPrefix([u8; GUID_PREFIX_LEN]);

impl GuidPrefix {
    /// `GUIDPREFIX_UNKNOWN`: twelve zero octets.
    ///
    /// A submessage that names this prefix as its destination is addressed to
    /// every participant that receives it (§8.3.7.8).
    pub const UNKNOWN: Self = Self([0; GUID_PREFIX_LEN]);

    /// Wrap twelve raw octets.
    #[must_use]
    pub const fn new(octets: [u8; GUID_PREFIX_LEN]) -> Self {
        Self(octets)
    }

    /// Build the prefix AstRS uses: the vendor id, then ten octets the caller
    /// makes unique.
    ///
    /// The ten octets are the participant's business — a host id, a process
    /// id and a counter in the classic layout, or ten octets of entropy. RTPS
    /// requires only that no two live participants collide.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_rtps::structure::{GuidPrefix, VendorId};
    ///
    /// let prefix = GuidPrefix::vendor_scoped(VendorId::ASTRS, [9; 10]);
    /// assert_eq!(prefix.vendor_id(), VendorId::ASTRS);
    /// assert_eq!(prefix.unique(), [9; 10]);
    /// ```
    #[must_use]
    pub const fn vendor_scoped(vendor: VendorId, unique: [u8; GUID_PREFIX_UNIQUE_LEN]) -> Self {
        let vendor = vendor.to_bytes();
        Self([
            vendor[0], vendor[1], unique[0], unique[1], unique[2], unique[3], unique[4], unique[5],
            unique[6], unique[7], unique[8], unique[9],
        ])
    }

    /// The twelve octets.
    #[must_use]
    pub const fn to_bytes(self) -> [u8; GUID_PREFIX_LEN] {
        self.0
    }

    /// The twelve octets, borrowed.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; GUID_PREFIX_LEN] {
        &self.0
    }

    /// The vendor id the first two octets carry (§9.3.1.5).
    #[must_use]
    pub const fn vendor_id(self) -> VendorId {
        VendorId::new([self.0[0], self.0[1]])
    }

    /// The ten octets after the vendor id.
    #[must_use]
    pub const fn unique(self) -> [u8; GUID_PREFIX_UNIQUE_LEN] {
        [
            self.0[2], self.0[3], self.0[4], self.0[5], self.0[6], self.0[7], self.0[8], self.0[9],
            self.0[10], self.0[11],
        ]
    }

    /// True for [`GuidPrefix::UNKNOWN`].
    #[must_use]
    pub const fn is_unknown(self) -> bool {
        let mut index = 0;
        while index < GUID_PREFIX_LEN {
            if self.0[index] != 0 {
                return false;
            }
            index += 1;
        }
        true
    }

    /// Attach an entity id, producing the full [`Guid`].
    #[must_use]
    pub const fn with_entity(self, entity_id: EntityId) -> Guid {
        Guid::new(self, entity_id)
    }

    /// The GUID of the participant this prefix names.
    #[must_use]
    pub const fn participant_guid(self) -> Guid {
        Guid::new(self, ENTITYID_PARTICIPANT)
    }

    /// Read twelve octets from the front of `bytes`.
    ///
    /// Returns `None` when fewer than twelve are available.
    #[must_use]
    pub fn from_slice(bytes: &[u8]) -> Option<Self> {
        let octets: [u8; GUID_PREFIX_LEN] = bytes.get(..GUID_PREFIX_LEN)?.try_into().ok()?;
        Some(Self(octets))
    }
}

impl fmt::Display for GuidPrefix {
    /// Dotted lower-case hex, the form a packet capture shows.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, octet) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str(".")?;
            }
            write!(f, "{octet:02x}")?;
        }
        Ok(())
    }
}

impl From<[u8; GUID_PREFIX_LEN]> for GuidPrefix {
    fn from(octets: [u8; GUID_PREFIX_LEN]) -> Self {
        Self(octets)
    }
}

impl From<GuidPrefix> for [u8; GUID_PREFIX_LEN] {
    fn from(prefix: GuidPrefix) -> Self {
        prefix.0
    }
}

impl CdrType for GuidPrefix {
    const MIN_SERIALIZED_SIZE: usize = GUID_PREFIX_LEN;
}

impl CdrSerialize for GuidPrefix {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        writer.write_octets(&self.0);
        Ok(())
    }
}

impl<'de> CdrDeserialize<'de> for GuidPrefix {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        let octets = reader.read_octets(GUID_PREFIX_LEN)?;
        let mut prefix = [0_u8; GUID_PREFIX_LEN];
        prefix.copy_from_slice(octets);
        Ok(Self(prefix))
    }
}

// ---------------------------------------------------------------------------
// EntityKind
// ---------------------------------------------------------------------------

/// Who defined an entity: the application, the protocol, or a vendor.
///
/// The two most significant bits of an [`EntityKind`] octet (§9.3.1.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum EntityCategory {
    /// `00` — created by the application through the DDS API.
    UserDefined,
    /// `01` — created by a vendor for a purpose outside the specification.
    VendorSpecific,
    /// `10` — not assigned by RTPS 2.3.
    Reserved,
    /// `11` — a built-in entity the protocol itself defines: the participant,
    /// the SPDP and SEDP endpoints, the liveliness endpoints.
    BuiltIn,
}

/// What an entity *is*: the low six bits of an [`EntityKind`] octet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum EntityRole {
    /// `0x00` — kind not stated.
    Unknown,
    /// `0x01` — a Participant.
    Participant,
    /// `0x02` — a Writer whose topic has a key.
    WriterWithKey,
    /// `0x03` — a Writer whose topic has no key.
    WriterNoKey,
    /// `0x04` — a Reader whose topic has no key.
    ReaderNoKey,
    /// `0x07` — a Reader whose topic has a key.
    ReaderWithKey,
    /// `0x08` — a group of Writers (a DDS `Publisher`).
    WriterGroup,
    /// `0x09` — a group of Readers (a DDS `Subscriber`).
    ReaderGroup,
    /// Any other six-bit value: reserved by RTPS 2.3.
    Other(u8),
}

/// The fourth octet of an [`EntityId`] (§9.3.1.2, Table 9.1).
///
/// The octet packs two independent facts: the top two bits are an
/// [`EntityCategory`], the low six an [`EntityRole`]. Both halves are
/// preserved verbatim, so an id from a vendor extension survives a decode and
/// re-encode unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct EntityKind(u8);

impl EntityKind {
    /// `0x00` — user-defined, kind unknown.
    pub const USER_UNKNOWN: Self = Self(0x00);
    /// `0x02` — user-defined Writer with a key.
    pub const USER_WRITER_WITH_KEY: Self = Self(0x02);
    /// `0x03` — user-defined Writer with no key.
    pub const USER_WRITER_NO_KEY: Self = Self(0x03);
    /// `0x04` — user-defined Reader with no key.
    pub const USER_READER_NO_KEY: Self = Self(0x04);
    /// `0x07` — user-defined Reader with a key.
    pub const USER_READER_WITH_KEY: Self = Self(0x07);
    /// `0x08` — user-defined Writer group.
    pub const USER_WRITER_GROUP: Self = Self(0x08);
    /// `0x09` — user-defined Reader group.
    pub const USER_READER_GROUP: Self = Self(0x09);
    /// `0xc0` — built-in, kind unknown.
    pub const BUILTIN_UNKNOWN: Self = Self(0xc0);
    /// `0xc1` — the built-in Participant.
    pub const BUILTIN_PARTICIPANT: Self = Self(0xc1);
    /// `0xc2` — built-in Writer with a key.
    pub const BUILTIN_WRITER_WITH_KEY: Self = Self(0xc2);
    /// `0xc3` — built-in Writer with no key.
    pub const BUILTIN_WRITER_NO_KEY: Self = Self(0xc3);
    /// `0xc4` — built-in Reader with no key.
    pub const BUILTIN_READER_NO_KEY: Self = Self(0xc4);
    /// `0xc7` — built-in Reader with a key.
    pub const BUILTIN_READER_WITH_KEY: Self = Self(0xc7);
    /// `0xc8` — built-in Writer group.
    pub const BUILTIN_WRITER_GROUP: Self = Self(0xc8);
    /// `0xc9` — built-in Reader group.
    pub const BUILTIN_READER_GROUP: Self = Self(0xc9);

    /// Mask of the two category bits.
    pub const CATEGORY_MASK: u8 = 0xc0;
    /// Mask of the six role bits.
    pub const ROLE_MASK: u8 = 0x3f;

    /// Wrap a raw octet.
    #[must_use]
    pub const fn new(octet: u8) -> Self {
        Self(octet)
    }

    /// The raw octet.
    #[must_use]
    pub const fn raw(self) -> u8 {
        self.0
    }

    /// Who defined the entity.
    #[must_use]
    pub const fn category(self) -> EntityCategory {
        match self.0 & Self::CATEGORY_MASK {
            0x00 => EntityCategory::UserDefined,
            0x40 => EntityCategory::VendorSpecific,
            0x80 => EntityCategory::Reserved,
            _ => EntityCategory::BuiltIn,
        }
    }

    /// What the entity is.
    #[must_use]
    pub const fn role(self) -> EntityRole {
        match self.0 & Self::ROLE_MASK {
            0x00 => EntityRole::Unknown,
            0x01 => EntityRole::Participant,
            0x02 => EntityRole::WriterWithKey,
            0x03 => EntityRole::WriterNoKey,
            0x04 => EntityRole::ReaderNoKey,
            0x07 => EntityRole::ReaderWithKey,
            0x08 => EntityRole::WriterGroup,
            0x09 => EntityRole::ReaderGroup,
            other => EntityRole::Other(other),
        }
    }

    /// True for a built-in entity (category `11`).
    #[must_use]
    pub const fn is_builtin(self) -> bool {
        matches!(self.category(), EntityCategory::BuiltIn)
    }

    /// True for an entity the application created (category `00`).
    #[must_use]
    pub const fn is_user_defined(self) -> bool {
        matches!(self.category(), EntityCategory::UserDefined)
    }

    /// True for a vendor extension (category `01`).
    #[must_use]
    pub const fn is_vendor_specific(self) -> bool {
        matches!(self.category(), EntityCategory::VendorSpecific)
    }

    /// True for either Writer role, at any category.
    #[must_use]
    pub const fn is_writer(self) -> bool {
        matches!(
            self.role(),
            EntityRole::WriterWithKey | EntityRole::WriterNoKey
        )
    }

    /// True for either Reader role, at any category.
    #[must_use]
    pub const fn is_reader(self) -> bool {
        matches!(
            self.role(),
            EntityRole::ReaderWithKey | EntityRole::ReaderNoKey
        )
    }

    /// True for a Writer or Reader *group* — a DDS `Publisher` or
    /// `Subscriber`.
    #[must_use]
    pub const fn is_group(self) -> bool {
        matches!(
            self.role(),
            EntityRole::WriterGroup | EntityRole::ReaderGroup
        )
    }

    /// True for the Participant role.
    #[must_use]
    pub const fn is_participant(self) -> bool {
        matches!(self.role(), EntityRole::Participant)
    }

    /// True when the entity's topic has a key, so its samples carry one.
    ///
    /// `None` for roles where the question does not apply.
    #[must_use]
    pub const fn has_key(self) -> Option<bool> {
        match self.role() {
            EntityRole::WriterWithKey | EntityRole::ReaderWithKey => Some(true),
            EntityRole::WriterNoKey | EntityRole::ReaderNoKey => Some(false),
            _ => None,
        }
    }
}

impl fmt::Display for EntityKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:02x}", self.0)
    }
}

impl From<u8> for EntityKind {
    fn from(octet: u8) -> Self {
        Self(octet)
    }
}

impl From<EntityKind> for u8 {
    fn from(kind: EntityKind) -> Self {
        kind.0
    }
}

// ---------------------------------------------------------------------------
// EntityId
// ---------------------------------------------------------------------------

/// The four octets that name an entity inside a participant (§9.3.1.2).
///
/// Three octets of `entityKey` chosen by the participant, then one octet of
/// [`EntityKind`]. Like every identifier here it is an octet sequence, not a
/// number, and has no byte order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct EntityId {
    /// The three key octets, most significant first.
    pub entity_key: [u8; 3],
    /// What kind of entity the key names.
    pub entity_kind: EntityKind,
}

impl EntityId {
    /// `ENTITYID_UNKNOWN`: four zero octets.
    pub const UNKNOWN: Self = Self::new([0, 0, 0], EntityKind::USER_UNKNOWN);

    /// Build an id from its two halves.
    #[must_use]
    pub const fn new(entity_key: [u8; 3], entity_kind: EntityKind) -> Self {
        Self {
            entity_key,
            entity_kind,
        }
    }

    /// Build an id from its four wire octets.
    #[must_use]
    pub const fn from_octets(octets: [u8; ENTITY_ID_LEN]) -> Self {
        Self::new(
            [octets[0], octets[1], octets[2]],
            EntityKind::new(octets[3]),
        )
    }

    /// The four wire octets.
    #[must_use]
    pub const fn to_octets(self) -> [u8; ENTITY_ID_LEN] {
        [
            self.entity_key[0],
            self.entity_key[1],
            self.entity_key[2],
            self.entity_kind.raw(),
        ]
    }

    /// Build a user-defined entity id from a counter.
    ///
    /// The counter fills the three key octets, most significant first; values
    /// above `0x00ff_ffff` wrap, which is the caller's problem to avoid.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_rtps::structure::{EntityId, EntityKind};
    ///
    /// let writer = EntityId::user_defined(7, EntityKind::USER_WRITER_NO_KEY);
    /// assert_eq!(writer.to_octets(), [0x00, 0x00, 0x07, 0x03]);
    /// assert!(writer.is_writer());
    /// assert!(!writer.is_builtin());
    /// ```
    #[must_use]
    pub const fn user_defined(counter: u32, kind: EntityKind) -> Self {
        let octets = counter.to_be_bytes();
        Self::new([octets[1], octets[2], octets[3]], kind)
    }

    /// The three key octets as a number, most significant first.
    #[must_use]
    pub const fn key_value(self) -> u32 {
        u32::from_be_bytes([
            0,
            self.entity_key[0],
            self.entity_key[1],
            self.entity_key[2],
        ])
    }

    /// True for [`EntityId::UNKNOWN`].
    #[must_use]
    pub const fn is_unknown(self) -> bool {
        self.entity_key[0] == 0
            && self.entity_key[1] == 0
            && self.entity_key[2] == 0
            && self.entity_kind.raw() == 0
    }

    /// True when the protocol, not the application, defines this entity.
    #[must_use]
    pub const fn is_builtin(self) -> bool {
        self.entity_kind.is_builtin()
    }

    /// True for either Writer role.
    #[must_use]
    pub const fn is_writer(self) -> bool {
        self.entity_kind.is_writer()
    }

    /// True for either Reader role.
    #[must_use]
    pub const fn is_reader(self) -> bool {
        self.entity_kind.is_reader()
    }

    /// True for the Participant role.
    #[must_use]
    pub const fn is_participant(self) -> bool {
        self.entity_kind.is_participant()
    }

    /// The specification's name for this id, when it is one of the well-known
    /// built-ins.
    ///
    /// Used by the receive path's log lines, where `ENTITYID_SPDP_BUILTIN_
    /// PARTICIPANT_WRITER` says far more than `00.01.00.c2`.
    #[must_use]
    pub fn well_known_name(self) -> Option<&'static str> {
        WELL_KNOWN
            .iter()
            .find(|(id, _)| *id == self)
            .map(|(_, name)| *name)
    }
}

impl fmt::Display for EntityId {
    /// The well-known name when there is one, else dotted hex.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(name) = self.well_known_name() {
            return f.write_str(name);
        }
        let octets = self.to_octets();
        write!(
            f,
            "{:02x}.{:02x}.{:02x}.{:02x}",
            octets[0], octets[1], octets[2], octets[3]
        )
    }
}

impl From<[u8; ENTITY_ID_LEN]> for EntityId {
    fn from(octets: [u8; ENTITY_ID_LEN]) -> Self {
        Self::from_octets(octets)
    }
}

impl From<EntityId> for [u8; ENTITY_ID_LEN] {
    fn from(id: EntityId) -> Self {
        id.to_octets()
    }
}

impl CdrType for EntityId {
    const MIN_SERIALIZED_SIZE: usize = ENTITY_ID_LEN;
}

impl CdrSerialize for EntityId {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        writer.write_octets(&self.to_octets());
        Ok(())
    }
}

impl<'de> CdrDeserialize<'de> for EntityId {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        let octets = reader.read_octets(ENTITY_ID_LEN)?;
        let mut buffer = [0_u8; ENTITY_ID_LEN];
        buffer.copy_from_slice(octets);
        Ok(Self::from_octets(buffer))
    }
}

// ---------------------------------------------------------------------------
// Well-known entity ids
// ---------------------------------------------------------------------------

/// `ENTITYID_UNKNOWN` (§9.3.1.4).
pub const ENTITYID_UNKNOWN: EntityId = EntityId::UNKNOWN;

/// `ENTITYID_PARTICIPANT` — the participant itself.
pub const ENTITYID_PARTICIPANT: EntityId =
    EntityId::new([0x00, 0x00, 0x01], EntityKind::BUILTIN_PARTICIPANT);

/// `ENTITYID_SEDP_BUILTIN_TOPIC_WRITER` — announces `DCPSTopic` samples.
pub const ENTITYID_SEDP_BUILTIN_TOPIC_WRITER: EntityId =
    EntityId::new([0x00, 0x00, 0x02], EntityKind::BUILTIN_WRITER_WITH_KEY);

/// `ENTITYID_SEDP_BUILTIN_TOPIC_READER` — receives `DCPSTopic` samples.
pub const ENTITYID_SEDP_BUILTIN_TOPIC_READER: EntityId =
    EntityId::new([0x00, 0x00, 0x02], EntityKind::BUILTIN_READER_WITH_KEY);

/// `ENTITYID_SEDP_BUILTIN_PUBLICATIONS_WRITER` — announces our writers.
pub const ENTITYID_SEDP_BUILTIN_PUBLICATIONS_WRITER: EntityId =
    EntityId::new([0x00, 0x00, 0x03], EntityKind::BUILTIN_WRITER_WITH_KEY);

/// `ENTITYID_SEDP_BUILTIN_PUBLICATIONS_READER` — learns a peer's writers.
pub const ENTITYID_SEDP_BUILTIN_PUBLICATIONS_READER: EntityId =
    EntityId::new([0x00, 0x00, 0x03], EntityKind::BUILTIN_READER_WITH_KEY);

/// `ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_WRITER` — announces our readers.
pub const ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_WRITER: EntityId =
    EntityId::new([0x00, 0x00, 0x04], EntityKind::BUILTIN_WRITER_WITH_KEY);

/// `ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_READER` — learns a peer's readers.
pub const ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_READER: EntityId =
    EntityId::new([0x00, 0x00, 0x04], EntityKind::BUILTIN_READER_WITH_KEY);

/// `ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER` — sends our SPDP announcement.
pub const ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER: EntityId =
    EntityId::new([0x00, 0x01, 0x00], EntityKind::BUILTIN_WRITER_WITH_KEY);

/// `ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER` — receives peers' SPDP
/// announcements.
pub const ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER: EntityId =
    EntityId::new([0x00, 0x01, 0x00], EntityKind::BUILTIN_READER_WITH_KEY);

/// `ENTITYID_P2P_BUILTIN_PARTICIPANT_MESSAGE_WRITER` — the Writer Liveliness
/// Protocol writer (§8.4.13).
pub const ENTITYID_P2P_BUILTIN_PARTICIPANT_MESSAGE_WRITER: EntityId =
    EntityId::new([0x00, 0x02, 0x00], EntityKind::BUILTIN_WRITER_WITH_KEY);

/// `ENTITYID_P2P_BUILTIN_PARTICIPANT_MESSAGE_READER` — the Writer Liveliness
/// Protocol reader (§8.4.13).
pub const ENTITYID_P2P_BUILTIN_PARTICIPANT_MESSAGE_READER: EntityId =
    EntityId::new([0x00, 0x02, 0x00], EntityKind::BUILTIN_READER_WITH_KEY);

/// Every well-known id with the name the specification gives it.
///
/// The ordering is the one §9.3.1.4 lists them in, and
/// [`EntityId::well_known_name`] is a linear scan of it — a dozen
/// comparisons, run once per log line, never on the data path.
pub const WELL_KNOWN: [(EntityId, &str); 12] = [
    (ENTITYID_UNKNOWN, "ENTITYID_UNKNOWN"),
    (ENTITYID_PARTICIPANT, "ENTITYID_PARTICIPANT"),
    (
        ENTITYID_SEDP_BUILTIN_TOPIC_WRITER,
        "ENTITYID_SEDP_BUILTIN_TOPIC_WRITER",
    ),
    (
        ENTITYID_SEDP_BUILTIN_TOPIC_READER,
        "ENTITYID_SEDP_BUILTIN_TOPIC_READER",
    ),
    (
        ENTITYID_SEDP_BUILTIN_PUBLICATIONS_WRITER,
        "ENTITYID_SEDP_BUILTIN_PUBLICATIONS_WRITER",
    ),
    (
        ENTITYID_SEDP_BUILTIN_PUBLICATIONS_READER,
        "ENTITYID_SEDP_BUILTIN_PUBLICATIONS_READER",
    ),
    (
        ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_WRITER,
        "ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_WRITER",
    ),
    (
        ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_READER,
        "ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_READER",
    ),
    (
        ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER,
        "ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER",
    ),
    (
        ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER,
        "ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER",
    ),
    (
        ENTITYID_P2P_BUILTIN_PARTICIPANT_MESSAGE_WRITER,
        "ENTITYID_P2P_BUILTIN_PARTICIPANT_MESSAGE_WRITER",
    ),
    (
        ENTITYID_P2P_BUILTIN_PARTICIPANT_MESSAGE_READER,
        "ENTITYID_P2P_BUILTIN_PARTICIPANT_MESSAGE_READER",
    ),
];

// ---------------------------------------------------------------------------
// Guid
// ---------------------------------------------------------------------------

/// The sixteen octets that name an entity globally (§9.3.1).
///
/// A GUID is a [`GuidPrefix`] and an [`EntityId`] written back to back, which
/// is exactly how it appears inside a `PID_PARTICIPANT_GUID` or
/// `PID_ENDPOINT_GUID` discovery parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Guid {
    /// The participant half.
    pub prefix: GuidPrefix,
    /// The entity half.
    pub entity_id: EntityId,
}

impl Guid {
    /// `GUID_UNKNOWN`: sixteen zero octets.
    pub const UNKNOWN: Self = Self::new(GuidPrefix::UNKNOWN, EntityId::UNKNOWN);

    /// Pair a prefix with an entity id.
    #[must_use]
    pub const fn new(prefix: GuidPrefix, entity_id: EntityId) -> Self {
        Self { prefix, entity_id }
    }

    /// The sixteen wire octets: prefix, then entity id.
    #[must_use]
    pub const fn to_bytes(self) -> [u8; GUID_LEN] {
        let prefix = self.prefix.to_bytes();
        let entity = self.entity_id.to_octets();
        [
            prefix[0], prefix[1], prefix[2], prefix[3], prefix[4], prefix[5], prefix[6], prefix[7],
            prefix[8], prefix[9], prefix[10], prefix[11], entity[0], entity[1], entity[2],
            entity[3],
        ]
    }

    /// Read the sixteen wire octets.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; GUID_LEN]) -> Self {
        Self::new(
            GuidPrefix::new([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
                bytes[8], bytes[9], bytes[10], bytes[11],
            ]),
            EntityId::from_octets([bytes[12], bytes[13], bytes[14], bytes[15]]),
        )
    }

    /// Read sixteen octets from the front of `bytes`.
    ///
    /// Returns `None` when fewer than sixteen are available. This is the door
    /// discovery uses on a `PID_ENDPOINT_GUID` parameter value.
    #[must_use]
    pub fn from_slice(bytes: &[u8]) -> Option<Self> {
        let octets: [u8; GUID_LEN] = bytes.get(..GUID_LEN)?.try_into().ok()?;
        Some(Self::from_bytes(octets))
    }

    /// True for [`Guid::UNKNOWN`].
    #[must_use]
    pub const fn is_unknown(self) -> bool {
        self.prefix.is_unknown() && self.entity_id.is_unknown()
    }

    /// The GUID of the participant this entity belongs to.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_rtps::structure::{ENTITYID_PARTICIPANT, GuidPrefix};
    ///
    /// let prefix = GuidPrefix::new([7; 12]);
    /// let writer = prefix.with_entity(astrs_rtps::structure::EntityId::user_defined(
    ///     1,
    ///     astrs_rtps::structure::EntityKind::USER_WRITER_NO_KEY,
    /// ));
    /// assert_eq!(writer.participant_guid().entity_id, ENTITYID_PARTICIPANT);
    /// ```
    #[must_use]
    pub const fn participant_guid(self) -> Self {
        Self::new(self.prefix, ENTITYID_PARTICIPANT)
    }

    /// True when both GUIDs name entities of the same participant.
    #[must_use]
    pub const fn same_participant(self, other: Self) -> bool {
        let (left, right) = (self.prefix.to_bytes(), other.prefix.to_bytes());
        let mut index = 0;
        while index < GUID_PREFIX_LEN {
            if left[index] != right[index] {
                return false;
            }
            index += 1;
        }
        true
    }
}

impl fmt::Display for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.prefix, self.entity_id)
    }
}

impl From<[u8; GUID_LEN]> for Guid {
    fn from(bytes: [u8; GUID_LEN]) -> Self {
        Self::from_bytes(bytes)
    }
}

impl From<Guid> for [u8; GUID_LEN] {
    fn from(guid: Guid) -> Self {
        guid.to_bytes()
    }
}

impl CdrType for Guid {
    const MIN_SERIALIZED_SIZE: usize = GUID_LEN;
}

impl CdrSerialize for Guid {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        writer.write_octets(&self.to_bytes());
        Ok(())
    }
}

impl<'de> CdrDeserialize<'de> for Guid {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        let octets = reader.read_octets(GUID_LEN)?;
        let mut buffer = [0_u8; GUID_LEN];
        buffer.copy_from_slice(octets);
        Ok(Self::from_bytes(buffer))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use astrs_cdr::{Encoding, from_bytes_headerless, to_vec_headerless};

    use super::*;

    #[test]
    fn a_prefix_carries_the_vendor_id_in_its_first_two_octets() {
        let prefix = GuidPrefix::vendor_scoped(VendorId::ASTRS, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        assert_eq!(
            prefix.to_bytes(),
            [0x41, 0x53, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10]
        );
        assert_eq!(prefix.vendor_id(), VendorId::ASTRS);
        assert_eq!(prefix.unique(), [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        assert!(!prefix.is_unknown());
        assert!(GuidPrefix::UNKNOWN.is_unknown());
        assert_eq!(GuidPrefix::default(), GuidPrefix::UNKNOWN);
    }

    #[test]
    fn a_prefix_prints_as_dotted_hex() {
        let prefix = GuidPrefix::new([0x01, 0x0f, 0x9c, 0x2e, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(prefix.to_string(), "01.0f.9c.2e.00.00.00.00.00.00.00.00");
    }

    #[test]
    fn a_prefix_reads_from_a_slice_only_when_twelve_octets_are_there() {
        assert!(GuidPrefix::from_slice(&[0_u8; 11]).is_none());
        assert_eq!(
            GuidPrefix::from_slice(&[3_u8; 16]),
            Some(GuidPrefix::new([3; 12]))
        );
    }

    #[test]
    fn the_well_known_ids_match_the_specification_octets() {
        // §9.3.1.4 gives these as literal four-octet values; each is asserted
        // here so a refactor of EntityKind cannot silently move one.
        assert_eq!(ENTITYID_UNKNOWN.to_octets(), [0x00, 0x00, 0x00, 0x00]);
        assert_eq!(ENTITYID_PARTICIPANT.to_octets(), [0x00, 0x00, 0x01, 0xc1]);
        assert_eq!(
            ENTITYID_SEDP_BUILTIN_TOPIC_WRITER.to_octets(),
            [0x00, 0x00, 0x02, 0xc2]
        );
        assert_eq!(
            ENTITYID_SEDP_BUILTIN_TOPIC_READER.to_octets(),
            [0x00, 0x00, 0x02, 0xc7]
        );
        assert_eq!(
            ENTITYID_SEDP_BUILTIN_PUBLICATIONS_WRITER.to_octets(),
            [0x00, 0x00, 0x03, 0xc2]
        );
        assert_eq!(
            ENTITYID_SEDP_BUILTIN_PUBLICATIONS_READER.to_octets(),
            [0x00, 0x00, 0x03, 0xc7]
        );
        assert_eq!(
            ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_WRITER.to_octets(),
            [0x00, 0x00, 0x04, 0xc2]
        );
        assert_eq!(
            ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_READER.to_octets(),
            [0x00, 0x00, 0x04, 0xc7]
        );
        assert_eq!(
            ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER.to_octets(),
            [0x00, 0x01, 0x00, 0xc2]
        );
        assert_eq!(
            ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER.to_octets(),
            [0x00, 0x01, 0x00, 0xc7]
        );
        assert_eq!(
            ENTITYID_P2P_BUILTIN_PARTICIPANT_MESSAGE_WRITER.to_octets(),
            [0x00, 0x02, 0x00, 0xc2]
        );
        assert_eq!(
            ENTITYID_P2P_BUILTIN_PARTICIPANT_MESSAGE_READER.to_octets(),
            [0x00, 0x02, 0x00, 0xc7]
        );
    }

    #[test]
    fn every_well_known_id_is_named_and_the_names_are_unique() {
        for (id, name) in WELL_KNOWN {
            assert_eq!(id.well_known_name(), Some(name));
            assert_eq!(id.to_string(), name);
        }
        for (index, (_, name)) in WELL_KNOWN.iter().enumerate() {
            assert!(
                !WELL_KNOWN[index + 1..]
                    .iter()
                    .any(|(_, other)| other == name),
                "{name} appears twice"
            );
        }
    }

    #[test]
    fn entity_kinds_split_into_a_category_and_a_role() {
        assert_eq!(
            EntityKind::BUILTIN_PARTICIPANT.category(),
            EntityCategory::BuiltIn
        );
        assert_eq!(
            EntityKind::BUILTIN_PARTICIPANT.role(),
            EntityRole::Participant
        );
        assert_eq!(
            EntityKind::USER_WRITER_NO_KEY.category(),
            EntityCategory::UserDefined
        );
        assert_eq!(
            EntityKind::new(0x43).category(),
            EntityCategory::VendorSpecific
        );
        assert_eq!(EntityKind::new(0x83).category(), EntityCategory::Reserved);
        assert_eq!(EntityKind::new(0x3f).role(), EntityRole::Other(0x3f));
        assert_eq!(EntityKind::BUILTIN_UNKNOWN.role(), EntityRole::Unknown);
        assert_eq!(
            EntityKind::BUILTIN_WRITER_GROUP.role(),
            EntityRole::WriterGroup
        );
        assert_eq!(
            EntityKind::USER_READER_GROUP.role(),
            EntityRole::ReaderGroup
        );
    }

    #[test]
    fn entity_kind_predicates_agree_with_the_table() {
        assert!(EntityKind::USER_WRITER_WITH_KEY.is_writer());
        assert!(EntityKind::BUILTIN_WRITER_NO_KEY.is_writer());
        assert!(EntityKind::USER_READER_NO_KEY.is_reader());
        assert!(EntityKind::BUILTIN_READER_WITH_KEY.is_reader());
        assert!(EntityKind::USER_WRITER_GROUP.is_group());
        assert!(EntityKind::BUILTIN_READER_GROUP.is_group());
        assert!(EntityKind::BUILTIN_PARTICIPANT.is_participant());
        assert!(EntityKind::USER_UNKNOWN.is_user_defined());
        assert!(EntityKind::new(0x42).is_vendor_specific());
        assert!(!EntityKind::USER_WRITER_WITH_KEY.is_builtin());

        assert_eq!(EntityKind::USER_WRITER_WITH_KEY.has_key(), Some(true));
        assert_eq!(EntityKind::USER_WRITER_NO_KEY.has_key(), Some(false));
        assert_eq!(EntityKind::BUILTIN_READER_WITH_KEY.has_key(), Some(true));
        assert_eq!(EntityKind::BUILTIN_READER_NO_KEY.has_key(), Some(false));
        assert_eq!(EntityKind::BUILTIN_PARTICIPANT.has_key(), None);
        assert_eq!(u8::from(EntityKind::from(0xc2_u8)), 0xc2);
        assert_eq!(EntityKind::BUILTIN_WRITER_WITH_KEY.to_string(), "0xc2");
    }

    #[test]
    fn user_defined_ids_pack_a_counter_into_the_key_octets() {
        let id = EntityId::user_defined(0x0012_3456, EntityKind::USER_READER_WITH_KEY);
        assert_eq!(id.to_octets(), [0x12, 0x34, 0x56, 0x07]);
        assert_eq!(id.key_value(), 0x0012_3456);
        assert!(id.is_reader());
        assert!(!id.is_writer());
        assert!(!id.is_participant());
        assert!(!id.is_builtin());
        assert!(!id.is_unknown());
        // The top octet of the counter is dropped: only three octets exist.
        assert_eq!(
            EntityId::user_defined(0xff00_0001, EntityKind::USER_WRITER_NO_KEY).key_value(),
            1
        );
    }

    #[test]
    fn an_unnamed_entity_id_prints_as_dotted_hex() {
        let id = EntityId::from_octets([0x00, 0x00, 0x09, 0x03]);
        assert_eq!(id.well_known_name(), None);
        assert_eq!(id.to_string(), "00.00.09.03");
        assert_eq!(<[u8; 4]>::from(id), [0x00, 0x00, 0x09, 0x03]);
        assert_eq!(EntityId::from([0x00, 0x00, 0x09, 0x03]), id);
    }

    #[test]
    fn a_guid_is_the_prefix_and_the_entity_id_back_to_back() {
        let prefix = GuidPrefix::new([0x41, 0x53, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        let guid = prefix.with_entity(ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER);
        assert_eq!(
            guid.to_bytes(),
            [
                0x41, 0x53, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 0x00, 0x01, 0x00, 0xc2
            ]
        );
        assert_eq!(Guid::from_bytes(guid.to_bytes()), guid);
        assert_eq!(Guid::from_slice(&guid.to_bytes()[..]), Some(guid));
        assert_eq!(Guid::from_slice(&[0_u8; 15]), None);
        assert_eq!(guid.participant_guid(), prefix.participant_guid());
        assert!(guid.same_participant(prefix.participant_guid()));
        assert!(!guid.same_participant(Guid::UNKNOWN));
        assert!(Guid::UNKNOWN.is_unknown());
        assert!(!guid.is_unknown());
        assert_eq!(Guid::default(), Guid::UNKNOWN);
        assert_eq!(
            guid.to_string(),
            "41.53.01.02.03.04.05.06.07.08.09.0a/ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER"
        );
    }

    #[test]
    fn identifiers_round_trip_through_cdr_in_both_byte_orders() {
        let guid = Guid::new(
            GuidPrefix::new([0x41, 0x53, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0]),
            ENTITYID_SEDP_BUILTIN_PUBLICATIONS_READER,
        );
        for encoding in [
            Encoding::ROS2,
            Encoding::new(astrs_cdr::EncapsulationKind::CdrBe),
        ] {
            let bytes = to_vec_headerless(&guid, encoding).expect("encode");
            // Octet sequences: identical octets in both byte orders.
            assert_eq!(bytes, guid.to_bytes());
            assert_eq!(
                from_bytes_headerless::<Guid>(&bytes, encoding).expect("decode"),
                guid
            );

            let prefix_bytes = to_vec_headerless(&guid.prefix, encoding).expect("encode");
            assert_eq!(
                from_bytes_headerless::<GuidPrefix>(&prefix_bytes, encoding).expect("decode"),
                guid.prefix
            );
            let entity_bytes = to_vec_headerless(&guid.entity_id, encoding).expect("encode");
            assert_eq!(
                from_bytes_headerless::<EntityId>(&entity_bytes, encoding).expect("decode"),
                guid.entity_id
            );
        }
        assert_eq!(Guid::from(<[u8; 16]>::from(guid)), guid);
    }
}
