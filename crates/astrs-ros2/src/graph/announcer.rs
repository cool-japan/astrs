//! [`GraphAnnouncer`]: publishing `ros_discovery_info` so `ros2 node list`
//! sees AstRS nodes.
//!
//! DDS discovery announces *endpoints*: a writer on `rt/chatter` carrying
//! `std_msgs::msg::dds_::String_`, owned by participant `01 0f …`. It says
//! nothing about *nodes*, because DDS has no such concept. Every ROS 2
//! `rmw` implementation therefore publishes a second, ROS-level graph over
//! one well-known topic:
//!
//! ```text
//!   topic     ros_discovery_info                     (no `rt/` prefix)
//!   type      rmw_dds_common::msg::dds_::ParticipantEntitiesInfo_
//!   qos       RELIABLE, TRANSIENT_LOCAL, KEEP_LAST(1)
//! ```
//!
//! One sample per participant, republished whenever its set of nodes or a
//! node's set of endpoints changes. `TRANSIENT_LOCAL` is what makes a
//! `ros2 node list` started *after* the node still see it — the sample is
//! replayed to the late joiner rather than waiting for the next change.
//!
//! # Why the topic is unprefixed
//!
//! Every other ROS topic is mangled to `rt/…`. This one is not, and cannot
//! be: it is the channel through which two implementations agree on what
//! nodes exist, so it must not depend on a convention either of them might
//! apply differently.
//! [`QosProfile::graph`](crate::qos::QosProfile::graph) is the one profile
//! in this crate that sets `avoid_ros_namespace_conventions`.
//!
//! # Republish-on-change, not on a timer
//!
//! Every mutating method republishes before it returns. That costs one
//! datagram per endpoint creation — a few dozen over a node's lifetime —
//! and it means a node's endpoints are visible in the graph the instant
//! they exist, with no cadence to wait for and no test that needs a sleep.

use astrs_rtps::behavior::{Participant, TopicKey, WriterHandle};
use astrs_rtps::discovery::{Gid, RosCompat, WriterQos as RtpsWriterQos};
use astrs_rtps::structure::Guid;
use tokio::sync::Mutex;

use crate::error::{Ros2Error, Ros2Result};
use crate::graph::wire::{NodeEntitiesInfo, ParticipantEntitiesInfo};
use crate::names::mangle::{GRAPH_TOPIC, GRAPH_TYPE};
use crate::qos::QosProfile;

/// Publishes this participant's `ros_discovery_info` sample.
///
/// One per [`Ros2Context`](crate::node::Ros2Context); every node on the
/// context registers its endpoints here.
#[derive(Debug)]
pub struct GraphAnnouncer {
    writer: WriterHandle,
    compat: RosCompat,
    state: Mutex<ParticipantEntitiesInfo>,
}

impl GraphAnnouncer {
    /// Create the announcer's writer and publish an empty sample.
    ///
    /// The empty sample matters: it tells a peer "this participant is an
    /// AstRS ROS participant that currently hosts no nodes", which is
    /// different from "this participant does not speak the ROS graph
    /// protocol at all".
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Rtps`] when the writer cannot be created, and
    /// [`Ros2Error::Cdr`] when the empty sample will not encode.
    pub async fn new(participant: &Participant, compat: RosCompat) -> Ros2Result<Self> {
        let key = TopicKey::new(GRAPH_TOPIC, GRAPH_TYPE).map_err(Ros2Error::Rtps)?;
        let qos: RtpsWriterQos = QosProfile::graph().to_writer_qos();
        let writer = participant
            .create_writer(key, qos)
            .await
            .map_err(Ros2Error::Rtps)?;
        let announcer = Self {
            writer,
            compat,
            state: Mutex::new(ParticipantEntitiesInfo::for_participant(
                compat,
                participant.guid(),
            )),
        };
        announcer.publish().await?;
        Ok(announcer)
    }

    /// The GID this participant announces itself under.
    pub async fn gid(&self) -> Gid {
        self.state.lock().await.gid
    }

    /// Which distribution's GID width this announcer writes.
    #[must_use]
    pub const fn compat(&self) -> RosCompat {
        self.compat
    }

    /// The writer's GUID, so a graph query can tell its own sample apart
    /// from a peer's.
    #[must_use]
    pub fn writer_guid(&self) -> Guid {
        self.writer.guid()
    }

    /// The sample as it currently stands.
    pub async fn snapshot(&self) -> ParticipantEntitiesInfo {
        self.state.lock().await.clone()
    }

    /// How many nodes this participant currently announces.
    pub async fn node_count(&self) -> usize {
        self.state.lock().await.nodes.len()
    }

    /// Add a node with no endpoints, and republish.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Cdr`] or [`Ros2Error::Rtps`].
    pub async fn add_node(&self, namespace: &str, name: &str) -> Ros2Result<()> {
        {
            let mut state = self.state.lock().await;
            let entry = NodeEntitiesInfo::new(namespace, name);
            if state.node(&entry.fully_qualified()).is_none() {
                state.upsert(entry);
            }
        }
        self.publish().await
    }

    /// Forget a node and every endpoint it owned, and republish.
    ///
    /// Returns whether the node was there.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Cdr`] or [`Ros2Error::Rtps`].
    pub async fn remove_node(&self, fully_qualified: &str) -> Ros2Result<bool> {
        let existed = self.state.lock().await.remove(fully_qualified);
        self.publish().await?;
        Ok(existed)
    }

    /// Record a publication under a node, and republish.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Cdr`] or [`Ros2Error::Rtps`].
    pub async fn add_writer(&self, fully_qualified: &str, writer: Guid) -> Ros2Result<()> {
        self.mutate(fully_qualified, |node, gid| node.add_writer(gid), writer)
            .await
    }

    /// Record a subscription under a node, and republish.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Cdr`] or [`Ros2Error::Rtps`].
    pub async fn add_reader(&self, fully_qualified: &str, reader: Guid) -> Ros2Result<()> {
        self.mutate(fully_qualified, |node, gid| node.add_reader(gid), reader)
            .await
    }

    /// Forget a publication, and republish.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Cdr`] or [`Ros2Error::Rtps`].
    pub async fn remove_writer(&self, fully_qualified: &str, writer: Guid) -> Ros2Result<()> {
        self.mutate(
            fully_qualified,
            |node, gid| {
                node.remove_writer(gid);
            },
            writer,
        )
        .await
    }

    /// Forget a subscription, and republish.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Cdr`] or [`Ros2Error::Rtps`].
    pub async fn remove_reader(&self, fully_qualified: &str, reader: Guid) -> Ros2Result<()> {
        self.mutate(
            fully_qualified,
            |node, gid| {
                node.remove_reader(gid);
            },
            reader,
        )
        .await
    }

    /// Apply a change to one node's entry and republish.
    ///
    /// A node that has not been added is added: an endpoint always belongs
    /// to a node, and losing the endpoint because the bookkeeping ran in the
    /// wrong order would make the graph lie.
    async fn mutate<F>(&self, fully_qualified: &str, change: F, endpoint: Guid) -> Ros2Result<()>
    where
        F: FnOnce(&mut NodeEntitiesInfo, Gid),
    {
        {
            let mut state = self.state.lock().await;
            let gid = Gid::new(self.compat, endpoint);
            let mut entry = state
                .node(fully_qualified)
                .cloned()
                .unwrap_or_else(|| split_name(fully_qualified));
            change(&mut entry, gid);
            state.upsert(entry);
        }
        self.publish().await
    }

    /// Encode the current sample and put it on the wire.
    async fn publish(&self) -> Ros2Result<()> {
        let payload = {
            let state = self.state.lock().await;
            state.encode(self.compat)?
        };
        self.writer.write(payload).await.map_err(Ros2Error::Rtps)?;
        Ok(())
    }
}

/// Split a fully-qualified node name into the namespace and name a
/// `NodeEntitiesInfo` carries separately.
///
/// `/robot/talker` is namespace `/robot`, name `talker`; `/talker` is
/// namespace `/`, name `talker`.
fn split_name(fully_qualified: &str) -> NodeEntitiesInfo {
    match fully_qualified.rsplit_once('/') {
        Some(("", name)) => NodeEntitiesInfo::new("/", name),
        Some((namespace, name)) => NodeEntitiesInfo::new(namespace, name),
        None => NodeEntitiesInfo::new("/", fully_qualified),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::node::{ContextOptions, Ros2Context};

    async fn announcer(compat: RosCompat) -> (std::sync::Arc<Ros2Context>, ()) {
        let context = Ros2Context::new(ContextOptions::loopback().with_compat(compat))
            .await
            .expect("bind");
        (context, ())
    }

    #[test]
    fn a_fully_qualified_name_splits_into_namespace_and_name() {
        let root = split_name("/talker");
        assert_eq!(root.node_namespace, "/");
        assert_eq!(root.node_name, "talker");
        assert_eq!(root.fully_qualified(), "/talker");

        let nested = split_name("/robot/arm/talker");
        assert_eq!(nested.node_namespace, "/robot/arm");
        assert_eq!(nested.node_name, "talker");
        assert_eq!(nested.fully_qualified(), "/robot/arm/talker");

        let bare = split_name("talker");
        assert_eq!(bare.node_namespace, "/");
        assert_eq!(bare.node_name, "talker");
    }

    #[tokio::test]
    async fn a_new_announcer_publishes_an_empty_sample() {
        let (context, ()) = announcer(RosCompat::Jazzy).await;
        let announcer = context.announcer().expect("on by default");
        let snapshot = announcer.snapshot().await;
        assert!(snapshot.nodes.is_empty());
        assert_eq!(snapshot.participant(), Some(context.guid()));
        assert_eq!(announcer.node_count().await, 0);
        context.shutdown().await;
    }

    #[tokio::test]
    async fn adding_a_node_twice_adds_it_once() {
        let (context, ()) = announcer(RosCompat::Jazzy).await;
        let announcer = context.announcer().expect("on");
        announcer.add_node("/robot", "talker").await.expect("add");
        announcer.add_node("/robot", "talker").await.expect("add");
        assert_eq!(announcer.node_count().await, 1);
        context.shutdown().await;
    }

    #[tokio::test]
    async fn endpoints_accumulate_under_their_node() {
        let (context, ()) = announcer(RosCompat::Jazzy).await;
        let announcer = context.announcer().expect("on");
        announcer.add_node("/", "talker").await.expect("add");

        let writer = context
            .create_writer("rt/a", "T", &QosProfile::default())
            .await
            .expect("writer");
        let reader = context
            .create_reader("rt/a", "T", &QosProfile::default())
            .await
            .expect("reader");
        announcer
            .add_writer("/talker", writer.guid())
            .await
            .expect("add");
        announcer
            .add_reader("/talker", reader.guid())
            .await
            .expect("add");

        let snapshot = announcer.snapshot().await;
        let node = snapshot.node("/talker").expect("present");
        assert_eq!(node.writer_gids.len(), 1);
        assert_eq!(node.reader_gids.len(), 1);
        assert!(node.owns(writer.guid()));
        assert!(node.owns(reader.guid()));
        context.shutdown().await;
    }

    #[tokio::test]
    async fn removing_an_endpoint_leaves_the_node() {
        let (context, ()) = announcer(RosCompat::Jazzy).await;
        let announcer = context.announcer().expect("on");
        let writer = context
            .create_writer("rt/b", "T", &QosProfile::default())
            .await
            .expect("writer");
        announcer
            .add_writer("/talker", writer.guid())
            .await
            .expect("add");
        announcer
            .remove_writer("/talker", writer.guid())
            .await
            .expect("remove");

        let snapshot = announcer.snapshot().await;
        assert_eq!(snapshot.nodes.len(), 1, "the node survives its endpoint");
        assert!(
            snapshot
                .node("/talker")
                .expect("present")
                .writer_gids
                .is_empty()
        );
        context.shutdown().await;
    }

    #[tokio::test]
    async fn an_endpoint_registered_before_its_node_creates_the_node() {
        let (context, ()) = announcer(RosCompat::Jazzy).await;
        let announcer = context.announcer().expect("on");
        let writer = context
            .create_writer("rt/c", "T", &QosProfile::default())
            .await
            .expect("writer");
        announcer
            .add_writer("/robot/talker", writer.guid())
            .await
            .expect("add");
        let snapshot = announcer.snapshot().await;
        let node = snapshot.node("/robot/talker").expect("created on demand");
        assert_eq!(node.node_namespace, "/robot");
        assert_eq!(node.writer_gids.len(), 1);
        context.shutdown().await;
    }

    #[tokio::test]
    async fn removing_a_node_reports_whether_it_was_there() {
        let (context, ()) = announcer(RosCompat::Jazzy).await;
        let announcer = context.announcer().expect("on");
        announcer.add_node("/", "talker").await.expect("add");
        assert!(announcer.remove_node("/talker").await.expect("remove"));
        assert!(!announcer.remove_node("/talker").await.expect("remove"));
        context.shutdown().await;
    }

    #[tokio::test]
    async fn the_announcer_writes_at_the_configured_gid_width() {
        for compat in RosCompat::ALL {
            let (context, ()) = announcer(compat).await;
            let announcer = context.announcer().expect("on");
            assert_eq!(announcer.compat(), compat);
            assert_eq!(announcer.gid().await.len(), compat.gid_len());

            announcer.add_node("/", "talker").await.expect("add");
            let snapshot = announcer.snapshot().await;
            let octets = snapshot.encode(compat).expect("encode");
            let decoded = ParticipantEntitiesInfo::decode(&octets, compat).expect("decode");
            assert_eq!(decoded, snapshot, "{compat} did not round trip");
            context.shutdown().await;
        }
    }

    #[tokio::test]
    async fn the_graph_writer_is_on_the_unprefixed_topic() {
        let (context, ()) = announcer(RosCompat::Jazzy).await;
        let announcer = context.announcer().expect("on");
        assert_eq!(announcer.writer_guid().prefix, context.guid().prefix);
        let db = context.discovery().await;
        assert_eq!(db.writer_count(), 0, "the local writer is not a remote one");
        context.shutdown().await;
    }
}
