//! Publishers: [`RawPublisher`] over octets, [`Publisher<T>`] over a type.

use core::marker::PhantomData;
use std::sync::Arc;

use astrs_rtps::behavior::WriterHandle;
use astrs_rtps::discovery::{Gid, RosCompat};
use astrs_rtps::structure::Guid;

use crate::MessageType;
use crate::error::{Ros2Error, Ros2Result};
use crate::names::{FullName, TopicKind};
use crate::node::Ros2Context;
use crate::qos::QosProfile;
use crate::time::RosTime;

/// A publication whose payloads are already CDR octets.
///
/// The type-erased half of the publisher API, and what a bridge uses: a
/// declarative `ros2:` block names a message type as a *string*, so the
/// bridge has octets and a type name rather than a Rust type. Everything
/// [`Publisher<T>`] does, it does through one of these.
#[derive(Debug, Clone)]
pub struct RawPublisher {
    inner: Arc<RawPublisherInner>,
}

#[derive(Debug)]
struct RawPublisherInner {
    context: Arc<Ros2Context>,
    writer: WriterHandle,
    topic: FullName,
    type_name: String,
    qos: QosProfile,
    /// The node whose graph entry owns this publisher, if it was created
    /// through one.
    owner: Option<String>,
}

impl RawPublisher {
    /// Create a publication on an expanded topic name.
    ///
    /// `dds_type` is the mangled type name — `sensor_msgs::msg::dds_::
    /// LaserScan_` — because that is what SEDP announces and what a peer
    /// matches on.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Rtps`] when the RTPS writer cannot be created.
    pub async fn new(
        context: Arc<Ros2Context>,
        topic: FullName,
        dds_type: impl Into<String>,
        qos: QosProfile,
    ) -> Ros2Result<Self> {
        Self::owned_by(context, topic, dds_type, qos, None).await
    }

    /// Create a publication whose DDS name carries a prefix other than
    /// `rt/`.
    ///
    /// The seam a service and an action need: a service's reply half is
    /// `rr/<name>Reply`, not `rt/<name>`, and mangling it as a topic would
    /// put every service on the wrong name — visible from outside only as a
    /// client that never matches a server.
    ///
    /// # Errors
    ///
    /// As [`owned_by`](Self::owned_by).
    pub async fn for_kind(
        context: Arc<Ros2Context>,
        topic: FullName,
        kind: TopicKind,
        dds_type: impl Into<String>,
        qos: QosProfile,
        owner: Option<String>,
    ) -> Ros2Result<Self> {
        let type_name = dds_type.into();
        let dds_topic = dds_topic_name_for(&topic, kind, &qos);
        let writer = context.create_writer(&dds_topic, &type_name, &qos).await?;

        if let (Some(node), Some(announcer)) = (owner.as_deref(), context.announcer()) {
            announcer.add_writer(node, writer.guid()).await?;
        }

        Ok(Self {
            inner: Arc::new(RawPublisherInner {
                context,
                writer,
                topic,
                type_name,
                qos,
                owner,
            }),
        })
    }

    /// Create a publication and register it under a node's graph entry.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Rtps`], or [`Ros2Error::Cdr`] when the graph sample will
    /// not encode.
    pub async fn owned_by(
        context: Arc<Ros2Context>,
        topic: FullName,
        dds_type: impl Into<String>,
        qos: QosProfile,
        owner: Option<String>,
    ) -> Ros2Result<Self> {
        Self::for_kind(context, topic, TopicKind::Topic, dds_type, qos, owner).await
    }

    /// The fully-qualified ROS topic name.
    #[must_use]
    pub fn topic(&self) -> &FullName {
        &self.inner.topic
    }

    /// The DDS topic name this publisher announces.
    #[must_use]
    pub fn dds_topic(&self) -> &str {
        &self.inner.writer.topic().topic_name
    }

    /// The DDS type name this publisher announces.
    #[must_use]
    pub fn type_name(&self) -> &str {
        &self.inner.type_name
    }

    /// The QoS profile it was created with.
    #[must_use]
    pub fn qos(&self) -> &QosProfile {
        &self.inner.qos
    }

    /// The RTPS writer's GUID.
    #[must_use]
    pub fn guid(&self) -> Guid {
        self.inner.writer.guid()
    }

    /// The GID a ROS graph query sees.
    #[must_use]
    pub fn gid(&self) -> Gid {
        Gid::new(self.compat(), self.guid())
    }

    /// The distribution this publisher's GID is encoded at.
    #[must_use]
    pub fn compat(&self) -> RosCompat {
        self.inner.context.compat()
    }

    /// The node that owns this publisher in the ROS graph, if any.
    #[must_use]
    pub fn owner(&self) -> Option<&str> {
        self.inner.owner.as_deref()
    }

    /// The context it belongs to.
    #[must_use]
    pub fn context(&self) -> &Arc<Ros2Context> {
        &self.inner.context
    }

    /// Put CDR octets on the wire, with the publisher's own timestamp.
    ///
    /// The write is on the wire before this returns — the RTPS layer sends
    /// inline rather than queueing — so a test that publishes and then
    /// awaits a subscription needs no sleep between them.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Rtps`] for a transport failure or a full history, and
    /// [`Ros2Error::NodeShutDown`] after shutdown.
    pub async fn publish_bytes(&self, payload: impl Into<Vec<u8>>) -> Ros2Result<()> {
        let stamp = self.inner.context.clock().now();
        self.publish_bytes_at(payload, stamp).await
    }

    /// Put CDR octets on the wire with an explicit source timestamp.
    ///
    /// What a replayer and a bridge both need: the stamp belongs to the
    /// sample, not to the moment it was forwarded.
    ///
    /// # Errors
    ///
    /// As [`publish_bytes`](Self::publish_bytes).
    pub async fn publish_bytes_at(
        &self,
        payload: impl Into<Vec<u8>>,
        stamp: RosTime,
    ) -> Ros2Result<()> {
        match self
            .inner
            .writer
            .write_at(payload.into(), stamp.to_rtps())
            .await
        {
            Ok(_) => Ok(()),
            Err(astrs_rtps::BehaviorError::Shutdown) => Err(Ros2Error::NodeShutDown),
            Err(error) => Err(Ros2Error::Rtps(error)),
        }
    }

    /// How many remote subscriptions this publisher has matched.
    pub async fn subscription_count(&self) -> usize {
        self.inner.writer.matched_readers().await
    }

    /// True when every matched reliable subscription has acknowledged
    /// everything written so far.
    pub async fn is_acknowledged(&self) -> bool {
        self.inner.writer.is_acknowledged().await
    }

    /// Wait until at least `count` subscriptions have matched.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Timeout`] when `timeout` elapses first.
    pub async fn wait_for_subscriptions(
        &self,
        count: usize,
        timeout: std::time::Duration,
    ) -> Ros2Result<()> {
        crate::pubsub::wait_for(
            &self.inner.context,
            timeout,
            "waiting for a subscription",
            || async { self.subscription_count().await >= count },
        )
        .await
    }

    /// Delete the publication, announcing its disposal over SEDP.
    ///
    /// Peers unwire immediately rather than waiting for the participant's
    /// lease. Also removes it from the node's graph entry.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Rtps`] or [`Ros2Error::Cdr`].
    pub async fn destroy(&self) -> Ros2Result<bool> {
        if let (Some(node), Some(announcer)) =
            (self.inner.owner.as_deref(), self.inner.context.announcer())
        {
            announcer.remove_writer(node, self.guid()).await?;
        }
        self.inner.context.delete_writer(self.guid()).await
    }
}

/// A publication of one Rust message type.
///
/// The typed half of the publisher API: the DDS type name comes from
/// [`MessageType`] rather than from a string a caller can misspell, and
/// `publish` takes a `&T` rather than octets.
#[derive(Debug)]
pub struct Publisher<T: MessageType> {
    raw: RawPublisher,
    marker: PhantomData<fn(&T)>,
}

impl<T: MessageType> Clone for Publisher<T> {
    fn clone(&self) -> Self {
        Self {
            raw: self.raw.clone(),
            marker: PhantomData,
        }
    }
}

impl<T: MessageType> Publisher<T> {
    /// Wrap a raw publisher whose type name is already `T`'s.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::DuplicateEntity`] when the raw publisher carries a
    /// different type name — a mismatch that would otherwise show up as a
    /// silent failure to match any subscription.
    pub fn from_raw(raw: RawPublisher) -> Ros2Result<Self> {
        if raw.type_name() != T::DDS_NAME {
            return Err(Ros2Error::DuplicateEntity {
                kind: "publisher",
                name: format!(
                    "{} carries {} but was asked for {}",
                    raw.topic(),
                    raw.type_name(),
                    T::DDS_NAME
                ),
            });
        }
        Ok(Self {
            raw,
            marker: PhantomData,
        })
    }

    /// Create a typed publication.
    ///
    /// # Errors
    ///
    /// As [`RawPublisher::owned_by`].
    pub async fn new(
        context: Arc<Ros2Context>,
        topic: FullName,
        qos: QosProfile,
        owner: Option<String>,
    ) -> Ros2Result<Self> {
        let raw = RawPublisher::owned_by(context, topic, T::DDS_NAME, qos, owner).await?;
        Ok(Self {
            raw,
            marker: PhantomData,
        })
    }

    /// The type-erased publisher underneath.
    #[must_use]
    pub const fn raw(&self) -> &RawPublisher {
        &self.raw
    }

    /// The fully-qualified ROS topic name.
    #[must_use]
    pub fn topic(&self) -> &FullName {
        self.raw.topic()
    }

    /// The QoS profile it was created with.
    #[must_use]
    pub fn qos(&self) -> &QosProfile {
        self.raw.qos()
    }

    /// The RTPS writer's GUID.
    #[must_use]
    pub fn guid(&self) -> Guid {
        self.raw.guid()
    }

    /// The GID a ROS graph query sees.
    #[must_use]
    pub fn gid(&self) -> Gid {
        self.raw.gid()
    }

    /// Encode and publish one message.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Cdr`] when `message` will not encode, plus whatever
    /// [`RawPublisher::publish_bytes`] reports.
    pub async fn publish(&self, message: &T) -> Ros2Result<()> {
        let payload = astrs_cdr::to_vec_ros2(message)?;
        self.raw.publish_bytes(payload).await
    }

    /// Encode and publish one message with an explicit source timestamp.
    ///
    /// # Errors
    ///
    /// As [`publish`](Self::publish).
    pub async fn publish_at(&self, message: &T, stamp: RosTime) -> Ros2Result<()> {
        let payload = astrs_cdr::to_vec_ros2(message)?;
        self.raw.publish_bytes_at(payload, stamp).await
    }

    /// How many remote subscriptions this publisher has matched.
    pub async fn subscription_count(&self) -> usize {
        self.raw.subscription_count().await
    }

    /// Wait until at least `count` subscriptions have matched.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Timeout`].
    pub async fn wait_for_subscriptions(
        &self,
        count: usize,
        timeout: std::time::Duration,
    ) -> Ros2Result<()> {
        self.raw.wait_for_subscriptions(count, timeout).await
    }

    /// Delete the publication.
    ///
    /// # Errors
    ///
    /// As [`RawPublisher::destroy`].
    pub async fn destroy(&self) -> Ros2Result<bool> {
        self.raw.destroy().await
    }
}

/// The DDS topic name a profile implies for a ROS name.
///
/// Almost always `rt/…`; the exception is a profile that sets
/// `avoid_ros_namespace_conventions`, which is how `ros_discovery_info` and
/// a bridge to a non-ROS DDS system both get an unmangled name.
///
/// Test-only: every production call site already has a specific
/// [`TopicKind`] in hand and goes straight to [`dds_topic_name_for`]; this
/// is the `TopicKind::Topic` convenience the tests below use.
#[cfg(test)]
fn dds_topic_name(topic: &FullName, qos: &QosProfile) -> String {
    dds_topic_name_for(topic, TopicKind::Topic, qos)
}

/// The DDS topic name a profile implies for a ROS name at `kind`.
pub(crate) fn dds_topic_name_for(topic: &FullName, kind: TopicKind, qos: &QosProfile) -> String {
    if qos.avoid_ros_namespace_conventions {
        topic
            .as_str()
            .strip_prefix('/')
            .unwrap_or(topic.as_str())
            .to_owned()
    } else {
        topic.dds_name(kind)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::msg::std_msgs;
    use crate::node::{ContextOptions, Ros2Context};

    async fn context() -> Arc<Ros2Context> {
        Ros2Context::new(ContextOptions::loopback())
            .await
            .expect("bind")
    }

    #[tokio::test]
    async fn a_typed_publisher_announces_the_mangled_names() {
        let context = context().await;
        let publisher = Publisher::<std_msgs::String>::new(
            Arc::clone(&context),
            FullName::topic("/chatter").expect("valid"),
            QosProfile::default(),
            None,
        )
        .await
        .expect("publisher");

        assert_eq!(publisher.topic().as_str(), "/chatter");
        assert_eq!(publisher.raw().dds_topic(), "rt/chatter");
        assert_eq!(publisher.raw().type_name(), "std_msgs::msg::dds_::String_");
        assert_eq!(publisher.subscription_count().await, 0);
        context.shutdown().await;
    }

    #[tokio::test]
    async fn publishing_before_anyone_listens_is_not_an_error() {
        let context = context().await;
        let publisher = Publisher::<std_msgs::String>::new(
            Arc::clone(&context),
            FullName::topic("/chatter").expect("valid"),
            QosProfile::default(),
            None,
        )
        .await
        .expect("publisher");
        publisher
            .publish(&std_msgs::String {
                data: "into the void".to_owned(),
            })
            .await
            .expect("a publisher with no subscribers still succeeds");
        context.shutdown().await;
    }

    #[tokio::test]
    async fn publishing_after_shutdown_reports_the_node_as_gone() {
        let context = context().await;
        let publisher = Publisher::<std_msgs::String>::new(
            Arc::clone(&context),
            FullName::topic("/chatter").expect("valid"),
            QosProfile::default(),
            None,
        )
        .await
        .expect("publisher");
        context.shutdown().await;
        let error = publisher
            .publish(&std_msgs::String {
                data: "too late".to_owned(),
            })
            .await
            .expect_err("refused");
        assert_eq!(error, Ros2Error::NodeShutDown);
        assert!(error.is_terminal());
    }

    #[tokio::test]
    async fn a_raw_publisher_takes_octets_and_a_type_name() {
        let context = context().await;
        let publisher = RawPublisher::new(
            Arc::clone(&context),
            FullName::topic("/chatter").expect("valid"),
            "std_msgs::msg::dds_::String_",
            QosProfile::default(),
        )
        .await
        .expect("publisher");

        let payload = astrs_cdr::to_vec_ros2(&std_msgs::String {
            data: "octets".to_owned(),
        })
        .expect("encode");
        publisher.publish_bytes(payload).await.expect("publish");
        assert!(publisher.owner().is_none());
        context.shutdown().await;
    }

    #[tokio::test]
    async fn wrapping_a_raw_publisher_checks_the_type_name() {
        let context = context().await;
        let raw = RawPublisher::new(
            Arc::clone(&context),
            FullName::topic("/chatter").expect("valid"),
            "std_msgs::msg::dds_::Int32_",
            QosProfile::default(),
        )
        .await
        .expect("publisher");

        let error = Publisher::<std_msgs::String>::from_raw(raw.clone())
            .expect_err("the type names disagree");
        assert!(matches!(error, Ros2Error::DuplicateEntity { .. }));

        let matching = RawPublisher::new(
            Arc::clone(&context),
            FullName::topic("/other").expect("valid"),
            "std_msgs::msg::dds_::String_",
            QosProfile::default(),
        )
        .await
        .expect("publisher");
        assert!(Publisher::<std_msgs::String>::from_raw(matching).is_ok());
        context.shutdown().await;
    }

    #[tokio::test]
    async fn a_graph_owning_publisher_registers_and_unregisters() {
        let context = context().await;
        let publisher = Publisher::<std_msgs::String>::new(
            Arc::clone(&context),
            FullName::topic("/chatter").expect("valid"),
            QosProfile::default(),
            Some("/talker".to_owned()),
        )
        .await
        .expect("publisher");

        let announcer = context.announcer().expect("on");
        assert!(
            announcer
                .snapshot()
                .await
                .node("/talker")
                .expect("created")
                .owns(publisher.guid())
        );

        publisher.destroy().await.expect("destroy");
        assert!(
            !announcer
                .snapshot()
                .await
                .node("/talker")
                .expect("still there")
                .owns(publisher.guid())
        );
        context.shutdown().await;
    }

    #[tokio::test]
    async fn a_profile_that_avoids_the_conventions_gets_an_unmangled_name() {
        let context = context().await;
        let publisher = RawPublisher::new(
            Arc::clone(&context),
            FullName::topic("/ros_discovery_info").expect("valid"),
            "rmw_dds_common::msg::dds_::ParticipantEntitiesInfo_",
            QosProfile::graph(),
        )
        .await
        .expect("publisher");
        assert_eq!(publisher.dds_topic(), "ros_discovery_info");
        context.shutdown().await;
    }

    #[test]
    fn the_topic_mangler_follows_the_profile() {
        let topic = FullName::topic("/robot/scan").expect("valid");
        assert_eq!(
            dds_topic_name(&topic, &QosProfile::default()),
            "rt/robot/scan"
        );
        assert_eq!(
            dds_topic_name(&topic, &QosProfile::graph()),
            "robot/scan",
            "the leading slash goes; the prefix does not arrive"
        );
    }

    #[tokio::test]
    async fn a_clone_is_the_same_publisher() {
        let context = context().await;
        let publisher = Publisher::<std_msgs::String>::new(
            Arc::clone(&context),
            FullName::topic("/chatter").expect("valid"),
            QosProfile::default(),
            None,
        )
        .await
        .expect("publisher");
        let clone = publisher.clone();
        assert_eq!(clone.guid(), publisher.guid());
        assert_eq!(clone.gid(), publisher.gid());
        assert_eq!(clone.qos(), publisher.qos());
        context.shutdown().await;
    }
}
