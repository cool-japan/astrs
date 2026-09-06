//! `rmw_dds_common`'s three graph types, hand-written because their width
//! is a runtime parameter.
//!
//! Every other ROS interface this crate needs is generated from `msg-src/`
//! ([`crate::interfaces`]). These three are not, and the reason is one
//! field:
//!
//! ```text
//!   rmw_dds_common/msg/Gid.msg   (Humble)   uint8[24] data
//!   rmw_dds_common/msg/Gid.msg   (Jazzy)    uint8[16] data
//! ```
//!
//! The array is *fixed-size*, so the wire carries no length and a decoder
//! has to be told which distribution it is talking to. A generated struct
//! has one width baked into its `CdrSerialize`/`CdrDeserialize` impls and
//! would silently mis-decode the other — the graph would look empty against
//! a Humble robot, or every node name would be shifted by eight octets.
//! Blueprint §10.2 makes the distribution a *per-bridge* value
//! ([`RosCompat`]), not a build feature, so the width has to be a runtime
//! parameter and these three types take it as an argument.
//!
//! The [`Gid`] value type itself is
//! `astrs-rtps`'s — a GID knows its own width — and
//! [`Gid::read_gid`](astrs_rtps::discovery::Gid::read_gid) is the
//! width-parameterized read this module composes with.
//!
//! # The topic
//!
//! `ros_discovery_info`, announced **unprefixed** and carrying
//! `rmw_dds_common::msg::dds_::ParticipantEntitiesInfo_`, reliable,
//! transient-local, keep-last 1. One sample per participant, rewritten
//! whenever the participant's set of nodes or a node's set of endpoints
//! changes. It is what makes `ros2 node list` show a *node* rather than a
//! participant: DDS discovery knows the endpoints, and this topic is the
//! only thing that says which node each endpoint belongs to.
//!
//! # Bounds
//!
//! Every sequence read here is bounded before a `Vec` grows: a peer's
//! announcement is untrusted input, and the alternative to
//! [`MAX_NODES_PER_PARTICIPANT`] is a length prefix from the network
//! deciding how much memory this process allocates.

use core::fmt;

use astrs_cdr::{CdrError, CdrReader, CdrResult, CdrWriter, Encoding};
use astrs_rtps::discovery::{Gid, RosCompat};
use astrs_rtps::structure::Guid;

/// The most nodes one participant may announce.
///
/// A process hosting more than this many ROS nodes is a component container
/// that has lost track of itself; either way the allocation has to stop
/// somewhere.
pub const MAX_NODES_PER_PARTICIPANT: usize = 1_024;

/// The most endpoints one node may announce, per direction.
pub const MAX_ENDPOINTS_PER_NODE: usize = 4_096;

/// The longest node name or namespace this decoder accepts.
pub const MAX_GRAPH_NAME_LEN: usize = 256;

/// The DDS type name the graph topic carries.
pub const PARTICIPANT_ENTITIES_INFO_TYPE: &str =
    "rmw_dds_common::msg::dds_::ParticipantEntitiesInfo_";

/// The DDS type name of one node's entry.
pub const NODE_ENTITIES_INFO_TYPE: &str = "rmw_dds_common::msg::dds_::NodeEntitiesInfo_";

/// The DDS type name of a GID.
pub const GID_TYPE: &str = "rmw_dds_common::msg::dds_::Gid_";

/// One ROS node, and every endpoint it owns.
///
/// `rmw_dds_common/msg/NodeEntitiesInfo`:
///
/// ```text
/// string node_namespace
/// string node_name
/// Gid[] reader_gid_seq
/// Gid[] writer_gid_seq
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NodeEntitiesInfo {
    /// The node's namespace, absolute.
    pub node_namespace: String,
    /// The node's name, without its namespace.
    pub node_name: String,
    /// Every subscription the node owns.
    pub reader_gids: Vec<Gid>,
    /// Every publisher the node owns.
    pub writer_gids: Vec<Gid>,
}

impl NodeEntitiesInfo {
    /// A node with no endpoints yet.
    #[must_use]
    pub fn new(node_namespace: impl Into<String>, node_name: impl Into<String>) -> Self {
        Self {
            node_namespace: node_namespace.into(),
            node_name: node_name.into(),
            reader_gids: Vec::new(),
            writer_gids: Vec::new(),
        }
    }

    /// The namespace and name joined, as `ros2 node list` prints it.
    #[must_use]
    pub fn fully_qualified(&self) -> String {
        crate::names::expand::join(&self.node_namespace, &self.node_name)
    }

    /// Record a subscription.
    pub fn add_reader(&mut self, gid: Gid) {
        if !self.reader_gids.contains(&gid) {
            self.reader_gids.push(gid);
        }
    }

    /// Record a publisher.
    pub fn add_writer(&mut self, gid: Gid) {
        if !self.writer_gids.contains(&gid) {
            self.writer_gids.push(gid);
        }
    }

    /// Forget a subscription. Returns whether it was there.
    pub fn remove_reader(&mut self, gid: Gid) -> bool {
        let before = self.reader_gids.len();
        self.reader_gids.retain(|existing| *existing != gid);
        self.reader_gids.len() != before
    }

    /// Forget a publisher. Returns whether it was there.
    pub fn remove_writer(&mut self, gid: Gid) -> bool {
        let before = self.writer_gids.len();
        self.writer_gids.retain(|existing| *existing != gid);
        self.writer_gids.len() != before
    }

    /// True when the node owns `guid`, in either direction.
    #[must_use]
    pub fn owns(&self, guid: Guid) -> bool {
        self.reader_gids
            .iter()
            .chain(self.writer_gids.iter())
            .any(|gid| gid.guid() == Some(guid))
    }

    /// How many endpoints the node owns.
    #[must_use]
    pub fn endpoint_count(&self) -> usize {
        self.reader_gids
            .len()
            .saturating_add(self.writer_gids.len())
    }

    /// Re-encode every GID at another distribution's width.
    #[must_use]
    pub fn to_compat(&self, compat: RosCompat) -> Self {
        Self {
            node_namespace: self.node_namespace.clone(),
            node_name: self.node_name.clone(),
            reader_gids: self
                .reader_gids
                .iter()
                .map(|gid| gid.to_compat(compat))
                .collect(),
            writer_gids: self
                .writer_gids
                .iter()
                .map(|gid| gid.to_compat(compat))
                .collect(),
        }
    }

    /// Write the body, without an encapsulation header.
    ///
    /// # Errors
    ///
    /// Whatever the writer reports.
    pub fn write(&self, writer: &mut CdrWriter, compat: RosCompat) -> CdrResult<()> {
        writer.write_str(&self.node_namespace)?;
        writer.write_str(&self.node_name)?;
        write_gid_sequence(writer, &self.reader_gids, compat)?;
        write_gid_sequence(writer, &self.writer_gids, compat)?;
        Ok(())
    }

    /// Read one body, without an encapsulation header.
    ///
    /// # Errors
    ///
    /// [`CdrError`] for malformed octets, and
    /// [`CdrError::LengthOverflow`] for a sequence longer than
    /// [`MAX_ENDPOINTS_PER_NODE`].
    pub fn read(reader: &mut CdrReader<'_>, compat: RosCompat) -> CdrResult<Self> {
        let node_namespace = read_bounded_string(reader, "NodeEntitiesInfo.node_namespace")?;
        let node_name = read_bounded_string(reader, "NodeEntitiesInfo.node_name")?;
        let reader_gids = read_gid_sequence(reader, compat, "NodeEntitiesInfo.reader_gid_seq")?;
        let writer_gids = read_gid_sequence(reader, compat, "NodeEntitiesInfo.writer_gid_seq")?;
        Ok(Self {
            node_namespace,
            node_name,
            reader_gids,
            writer_gids,
        })
    }
}

impl fmt::Display for NodeEntitiesInfo {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} ({} pub, {} sub)",
            self.fully_qualified(),
            self.writer_gids.len(),
            self.reader_gids.len()
        )
    }
}

/// One participant's whole contribution to the ROS graph.
///
/// `rmw_dds_common/msg/ParticipantEntitiesInfo`:
///
/// ```text
/// Gid gid
/// NodeEntitiesInfo[] node_entities_info_seq
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParticipantEntitiesInfo {
    /// The participant this sample describes.
    pub gid: Gid,
    /// Every ROS node it hosts.
    pub nodes: Vec<NodeEntitiesInfo>,
}

impl ParticipantEntitiesInfo {
    /// A participant hosting no nodes yet.
    #[must_use]
    pub const fn new(gid: Gid) -> Self {
        Self {
            gid,
            nodes: Vec::new(),
        }
    }

    /// A participant announcement for `guid` at `compat`'s width.
    #[must_use]
    pub const fn for_participant(compat: RosCompat, guid: Guid) -> Self {
        Self::new(Gid::new(compat, guid))
    }

    /// Which distribution's width this sample was written at, when the
    /// width is one of the two.
    #[must_use]
    pub const fn compat(&self) -> Option<RosCompat> {
        self.gid.compat()
    }

    /// The participant's GUID, or `None` for the all-zero "unknown" GID.
    #[must_use]
    pub fn participant(&self) -> Option<Guid> {
        self.gid.guid()
    }

    /// The node with this fully-qualified name, if the participant hosts it.
    #[must_use]
    pub fn node(&self, fully_qualified: &str) -> Option<&NodeEntitiesInfo> {
        self.nodes
            .iter()
            .find(|node| node.fully_qualified() == fully_qualified)
    }

    /// The node that owns `guid`, if any.
    #[must_use]
    pub fn owner_of(&self, guid: Guid) -> Option<&NodeEntitiesInfo> {
        self.nodes.iter().find(|node| node.owns(guid))
    }

    /// Add or replace a node's entry.
    ///
    /// Replacement rather than duplication, because the sample is a
    /// *snapshot*: two entries for one node name would make a reader show
    /// the node twice.
    pub fn upsert(&mut self, node: NodeEntitiesInfo) {
        let name = node.fully_qualified();
        match self
            .nodes
            .iter_mut()
            .find(|existing| existing.fully_qualified() == name)
        {
            Some(existing) => *existing = node,
            None => self.nodes.push(node),
        }
    }

    /// Remove a node's entry. Returns whether it was there.
    pub fn remove(&mut self, fully_qualified: &str) -> bool {
        let before = self.nodes.len();
        self.nodes
            .retain(|node| node.fully_qualified() != fully_qualified);
        self.nodes.len() != before
    }

    /// Re-encode the whole sample at another distribution's width.
    ///
    /// What a bridge does when it forwards a Humble robot's graph to a
    /// Jazzy one.
    #[must_use]
    pub fn to_compat(&self, compat: RosCompat) -> Self {
        Self {
            gid: self.gid.to_compat(compat),
            nodes: self
                .nodes
                .iter()
                .map(|node| node.to_compat(compat))
                .collect(),
        }
    }

    /// Encode a whole sample, encapsulation header included.
    ///
    /// # Errors
    ///
    /// Whatever the writer reports.
    pub fn encode(&self, compat: RosCompat) -> CdrResult<Vec<u8>> {
        let mut writer = CdrWriter::new(Encoding::ROS2);
        writer.write_octets(self.gid.as_slice());
        writer.write_sequence_len(self.nodes.len())?;
        for node in &self.nodes {
            node.write(&mut writer, compat)?;
        }
        Ok(writer.finish())
    }

    /// Decode a whole sample, encapsulation header included.
    ///
    /// # Errors
    ///
    /// [`CdrError`] for malformed octets, and
    /// [`CdrError::LengthOverflow`] for more than
    /// [`MAX_NODES_PER_PARTICIPANT`] nodes.
    pub fn decode(payload: &[u8], compat: RosCompat) -> CdrResult<Self> {
        let mut reader = CdrReader::new(payload)?;
        let gid = Gid::read_gid(&mut reader, compat)?;
        let count =
            reader.read_sequence_len(1, "ParticipantEntitiesInfo.node_entities_info_seq")?;
        if count > MAX_NODES_PER_PARTICIPANT {
            return Err(CdrError::LengthOverflow {
                declared: count as u64,
                available: MAX_NODES_PER_PARTICIPANT,
                element_size: 1,
                context: "ParticipantEntitiesInfo.node_entities_info_seq",
            });
        }
        let mut nodes = Vec::with_capacity(count.min(64));
        for _ in 0..count {
            nodes.push(NodeEntitiesInfo::read(&mut reader, compat)?);
        }
        Ok(Self { gid, nodes })
    }
}

impl fmt::Display for ParticipantEntitiesInfo {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} hosting {} node(s)",
            self.gid,
            self.nodes.len()
        )
    }
}

/// Write a `Gid[]` at `compat`'s width.
fn write_gid_sequence(writer: &mut CdrWriter, gids: &[Gid], compat: RosCompat) -> CdrResult<()> {
    writer.write_sequence_len(gids.len())?;
    for gid in gids {
        // A GID carries its own width, and a sequence must be homogeneous:
        // re-encoding at the sample's width is what keeps a bridge from
        // writing a Humble GID into a Jazzy sample.
        writer.write_octets(gid.to_compat(compat).as_slice());
    }
    Ok(())
}

/// Read a `Gid[]` at `compat`'s width, bounded.
fn read_gid_sequence(
    reader: &mut CdrReader<'_>,
    compat: RosCompat,
    context: &'static str,
) -> CdrResult<Vec<Gid>> {
    let count = reader.read_sequence_len(compat.gid_len(), context)?;
    if count > MAX_ENDPOINTS_PER_NODE {
        return Err(CdrError::LengthOverflow {
            declared: count as u64,
            available: MAX_ENDPOINTS_PER_NODE,
            element_size: compat.gid_len(),
            context,
        });
    }
    let mut gids = Vec::with_capacity(count.min(64));
    for _ in 0..count {
        gids.push(Gid::read_gid(reader, compat)?);
    }
    Ok(gids)
}

/// Read a string, refusing one longer than [`MAX_GRAPH_NAME_LEN`].
fn read_bounded_string(reader: &mut CdrReader<'_>, context: &'static str) -> CdrResult<String> {
    let text = reader.read_str()?;
    if text.len() > MAX_GRAPH_NAME_LEN {
        return Err(CdrError::LengthOverflow {
            declared: text.len() as u64,
            available: MAX_GRAPH_NAME_LEN,
            element_size: 1,
            context,
        });
    }
    Ok(text.to_owned())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use astrs_rtps::structure::{ENTITYID_PARTICIPANT, EntityId, EntityKind, GuidPrefix, VendorId};

    fn guid(seed: u8, key: u32) -> Guid {
        Guid::new(
            GuidPrefix::vendor_scoped(VendorId::ASTRS, [seed; 10]),
            if key == 0 {
                ENTITYID_PARTICIPANT
            } else {
                EntityId::user_defined(key, EntityKind::USER_WRITER_NO_KEY)
            },
        )
    }

    fn sample(compat: RosCompat) -> ParticipantEntitiesInfo {
        let mut info = ParticipantEntitiesInfo::for_participant(compat, guid(1, 0));
        let mut talker = NodeEntitiesInfo::new("/robot", "talker");
        talker.add_writer(Gid::new(compat, guid(1, 1)));
        talker.add_reader(Gid::new(compat, guid(1, 2)));
        let mut listener = NodeEntitiesInfo::new("/", "listener");
        listener.add_reader(Gid::new(compat, guid(1, 3)));
        info.upsert(talker);
        info.upsert(listener);
        info
    }

    #[test]
    fn a_sample_round_trips_at_both_widths() {
        for compat in RosCompat::ALL {
            let original = sample(compat);
            let octets = original.encode(compat).expect("encode");
            let decoded = ParticipantEntitiesInfo::decode(&octets, compat).expect("decode");
            assert_eq!(decoded, original, "{compat} did not round trip");
            assert_eq!(decoded.compat(), Some(compat));
        }
    }

    #[test]
    fn the_two_widths_produce_different_octet_counts() {
        let humble = sample(RosCompat::Humble)
            .encode(RosCompat::Humble)
            .expect("encode");
        let jazzy = sample(RosCompat::Jazzy)
            .encode(RosCompat::Jazzy)
            .expect("encode");
        assert!(
            humble.len() > jazzy.len(),
            "Humble pads every one of the four GIDs by eight octets: \
             {} vs {}",
            humble.len(),
            jazzy.len()
        );
        assert_eq!(humble.len() - jazzy.len(), 8 * 4);
    }

    #[test]
    fn decoding_at_the_wrong_width_does_not_silently_succeed() {
        let humble = sample(RosCompat::Humble)
            .encode(RosCompat::Humble)
            .expect("encode");
        let misread = ParticipantEntitiesInfo::decode(&humble, RosCompat::Jazzy);
        match misread {
            Err(_) => {}
            Ok(decoded) => assert_ne!(
                decoded,
                sample(RosCompat::Jazzy),
                "reading a 24-octet GID as 16 must not produce the Jazzy sample"
            ),
        }
    }

    #[test]
    fn a_sample_re_encodes_between_distributions() {
        let humble = sample(RosCompat::Humble);
        let bridged = humble.to_compat(RosCompat::Jazzy);
        assert_eq!(bridged.compat(), Some(RosCompat::Jazzy));
        assert_eq!(bridged.participant(), humble.participant());
        assert_eq!(bridged.nodes.len(), humble.nodes.len());

        let octets = bridged.encode(RosCompat::Jazzy).expect("encode");
        let decoded = ParticipantEntitiesInfo::decode(&octets, RosCompat::Jazzy).expect("decode");
        assert_eq!(decoded, bridged);
        assert_eq!(
            decoded.nodes[0].writer_gids[0].guid(),
            humble.nodes[0].writer_gids[0].guid(),
            "the GUID survives the width change"
        );
    }

    #[test]
    fn a_node_joins_its_namespace_and_name() {
        assert_eq!(
            NodeEntitiesInfo::new("/robot", "talker").fully_qualified(),
            "/robot/talker"
        );
        assert_eq!(
            NodeEntitiesInfo::new("/", "listener").fully_qualified(),
            "/listener"
        );
    }

    #[test]
    fn endpoints_are_added_once_and_removed_once() {
        let mut node = NodeEntitiesInfo::new("/", "talker");
        let gid = Gid::new(RosCompat::Jazzy, guid(2, 1));
        node.add_writer(gid);
        node.add_writer(gid);
        assert_eq!(node.writer_gids.len(), 1, "adding twice adds once");
        assert_eq!(node.endpoint_count(), 1);
        assert!(node.remove_writer(gid));
        assert!(!node.remove_writer(gid));
        assert_eq!(node.endpoint_count(), 0);

        node.add_reader(gid);
        assert!(node.remove_reader(gid));
        assert!(!node.remove_reader(gid));
    }

    #[test]
    fn a_node_knows_which_endpoints_it_owns() {
        let mut node = NodeEntitiesInfo::new("/", "talker");
        node.add_writer(Gid::new(RosCompat::Jazzy, guid(3, 1)));
        assert!(node.owns(guid(3, 1)));
        assert!(!node.owns(guid(3, 2)));
    }

    #[test]
    fn upserting_replaces_rather_than_duplicating() {
        let mut info = ParticipantEntitiesInfo::for_participant(RosCompat::Jazzy, guid(4, 0));
        info.upsert(NodeEntitiesInfo::new("/", "talker"));
        let mut updated = NodeEntitiesInfo::new("/", "talker");
        updated.add_writer(Gid::new(RosCompat::Jazzy, guid(4, 1)));
        info.upsert(updated);

        assert_eq!(info.nodes.len(), 1, "a snapshot holds one entry per node");
        assert_eq!(info.node("/talker").expect("present").endpoint_count(), 1);
        assert!(info.remove("/talker"));
        assert!(!info.remove("/talker"));
        assert!(info.nodes.is_empty());
    }

    #[test]
    fn a_participant_finds_the_node_that_owns_an_endpoint() {
        let info = sample(RosCompat::Jazzy);
        assert_eq!(
            info.owner_of(guid(1, 1))
                .map(NodeEntitiesInfo::fully_qualified),
            Some("/robot/talker".to_owned())
        );
        assert_eq!(
            info.owner_of(guid(1, 3))
                .map(NodeEntitiesInfo::fully_qualified),
            Some("/listener".to_owned())
        );
        assert!(info.owner_of(guid(9, 9)).is_none());
    }

    #[test]
    fn an_absurd_node_count_is_refused_before_a_vec_grows() {
        // A hand-built sample: a valid GID, then a sequence length of four
        // billion. The `read_sequence_len` bound catches it first because the
        // buffer is short; the explicit ceiling catches the case where a peer
        // sends a long-but-plausible buffer.
        let mut writer = CdrWriter::new(Encoding::ROS2);
        writer.write_octets(Gid::new(RosCompat::Jazzy, guid(5, 0)).as_slice());
        writer.write_length(u32::MAX).expect("length");
        let octets = writer.finish();
        assert!(ParticipantEntitiesInfo::decode(&octets, RosCompat::Jazzy).is_err());
    }

    #[test]
    fn an_absurd_endpoint_count_is_refused() {
        let mut writer = CdrWriter::new(Encoding::ROS2);
        writer.write_octets(Gid::new(RosCompat::Jazzy, guid(6, 0)).as_slice());
        writer.write_sequence_len(1).expect("one node");
        writer.write_str("/").expect("namespace");
        writer.write_str("talker").expect("name");
        writer.write_length(u32::MAX).expect("length");
        let octets = writer.finish();
        assert!(ParticipantEntitiesInfo::decode(&octets, RosCompat::Jazzy).is_err());
    }

    #[test]
    fn an_over_long_node_name_is_refused() {
        let mut writer = CdrWriter::new(Encoding::ROS2);
        writer.write_octets(Gid::new(RosCompat::Jazzy, guid(7, 0)).as_slice());
        writer.write_sequence_len(1).expect("one node");
        writer.write_str("/").expect("namespace");
        writer
            .write_str(&"n".repeat(MAX_GRAPH_NAME_LEN + 1))
            .expect("name");
        writer.write_sequence_len(0).expect("readers");
        writer.write_sequence_len(0).expect("writers");
        let octets = writer.finish();
        assert!(ParticipantEntitiesInfo::decode(&octets, RosCompat::Jazzy).is_err());
    }

    #[test]
    fn an_empty_participant_encodes_and_decodes() {
        for compat in RosCompat::ALL {
            let info = ParticipantEntitiesInfo::for_participant(compat, guid(8, 0));
            let octets = info.encode(compat).expect("encode");
            assert_eq!(
                ParticipantEntitiesInfo::decode(&octets, compat).expect("decode"),
                info
            );
            assert!(info.nodes.is_empty());
            assert_eq!(info.participant(), Some(guid(8, 0)));
        }
    }

    #[test]
    fn the_display_forms_name_what_matters() {
        let info = sample(RosCompat::Jazzy);
        assert!(info.to_string().contains("2 node(s)"), "{info}");
        let node = &info.nodes[0];
        assert!(node.to_string().contains("/robot/talker"), "{node}");
        assert!(node.to_string().contains("1 pub"), "{node}");
    }

    #[test]
    fn the_type_names_match_the_mangler() {
        use crate::names::mangle::{TypeNamespace, dds_type_name};
        assert_eq!(
            PARTICIPANT_ENTITIES_INFO_TYPE,
            dds_type_name(
                "rmw_dds_common",
                TypeNamespace::Msg,
                "ParticipantEntitiesInfo",
                ""
            )
        );
        assert_eq!(
            NODE_ENTITIES_INFO_TYPE,
            dds_type_name("rmw_dds_common", TypeNamespace::Msg, "NodeEntitiesInfo", "")
        );
        assert_eq!(
            GID_TYPE,
            dds_type_name("rmw_dds_common", TypeNamespace::Msg, "Gid", "")
        );
        assert_eq!(
            crate::names::mangle::GRAPH_TYPE,
            PARTICIPANT_ENTITIES_INFO_TYPE
        );
    }
}
