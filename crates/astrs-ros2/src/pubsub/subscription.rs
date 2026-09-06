//! Subscriptions: [`RawSubscription`] over octets, [`Subscription<T>`] over
//! a type, and the [`MessageInfo`] that rides beside every sample.

use core::marker::PhantomData;
use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant};

use astrs_rtps::behavior::{ReaderHandle, Sample};
use astrs_rtps::discovery::{Gid, RosCompat};
use astrs_rtps::structure::Guid;

use crate::MessageType;
use crate::error::{Ros2Error, Ros2Result};
use crate::names::FullName;
use crate::node::Ros2Context;
use crate::qos::QosProfile;
use crate::time::RosTime;

/// What DDS knows about a received sample beyond its payload.
///
/// `rmw`'s `rmw_message_info_t`, with the fields RTPS can actually supply.
/// The publisher's GID is the field a service reply and an action feedback
/// message both correlate on, which is why it is here rather than behind a
/// method on the subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageInfo {
    /// The publisher that wrote it.
    pub publisher: Guid,
    /// The publisher, at the configured GID width.
    pub publisher_gid: Gid,
    /// The writer's RTPS sequence number.
    pub sequence_number: i64,
    /// The `INFO_TS` the publisher sent, if it sent one.
    pub source_timestamp: Option<RosTime>,
    /// When this host accepted it.
    pub received_at: Instant,
    /// False when the sample is a disposal rather than data.
    pub is_alive: bool,
}

impl MessageInfo {
    /// Read the info out of an RTPS sample.
    #[must_use]
    pub fn from_sample(sample: &Sample, compat: RosCompat) -> Self {
        Self {
            publisher: sample.writer,
            publisher_gid: Gid::new(compat, sample.writer),
            sequence_number: sample.sequence_number.value(),
            source_timestamp: sample.source_timestamp.map(RosTime::from_rtps),
            received_at: sample.received_at,
            is_alive: sample.is_alive(),
        }
    }
}

/// A subscription whose payloads stay CDR octets.
///
/// The type-erased half: a bridge that was handed `"sensor_msgs/msg/
/// LaserScan"` as a string uses this, and [`Subscription<T>`] is a thin
/// decoding wrapper over it.
#[derive(Debug, Clone)]
pub struct RawSubscription {
    inner: Arc<RawSubscriptionInner>,
}

#[derive(Debug)]
struct RawSubscriptionInner {
    context: Arc<Ros2Context>,
    reader: ReaderHandle,
    topic: FullName,
    type_name: String,
    qos: QosProfile,
    owner: Option<String>,
}

impl RawSubscription {
    /// Create a subscription on an expanded topic name.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Rtps`] when the RTPS reader cannot be created.
    pub async fn new(
        context: Arc<Ros2Context>,
        topic: FullName,
        dds_type: impl Into<String>,
        qos: QosProfile,
    ) -> Ros2Result<Self> {
        Self::owned_by(context, topic, dds_type, qos, None).await
    }

    /// Create a subscription whose DDS name carries a prefix other than
    /// `rt/`.
    ///
    /// The counterpart of
    /// [`RawPublisher::for_kind`](crate::pubsub::RawPublisher::for_kind):
    /// a service server subscribes to `rq/<name>Request`.
    ///
    /// # Errors
    ///
    /// As [`owned_by`](Self::owned_by).
    pub async fn for_kind(
        context: Arc<Ros2Context>,
        topic: FullName,
        kind: crate::names::TopicKind,
        dds_type: impl Into<String>,
        qos: QosProfile,
        owner: Option<String>,
    ) -> Ros2Result<Self> {
        let type_name = dds_type.into();
        let dds_topic = super::publisher::dds_topic_name_for(&topic, kind, &qos);
        let reader = context.create_reader(&dds_topic, &type_name, &qos).await?;

        if let (Some(node), Some(announcer)) = (owner.as_deref(), context.announcer()) {
            announcer.add_reader(node, reader.guid()).await?;
        }

        Ok(Self {
            inner: Arc::new(RawSubscriptionInner {
                context,
                reader,
                topic,
                type_name,
                qos,
                owner,
            }),
        })
    }

    /// Create a subscription and register it under a node's graph entry.
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
        Self::for_kind(
            context,
            topic,
            crate::names::TopicKind::Topic,
            dds_type,
            qos,
            owner,
        )
        .await
    }

    /// The fully-qualified ROS topic name.
    #[must_use]
    pub fn topic(&self) -> &FullName {
        &self.inner.topic
    }

    /// The DDS topic name this subscription announces.
    #[must_use]
    pub fn dds_topic(&self) -> &str {
        &self.inner.reader.topic().topic_name
    }

    /// The DDS type name it announces.
    #[must_use]
    pub fn type_name(&self) -> &str {
        &self.inner.type_name
    }

    /// The QoS profile it was created with.
    #[must_use]
    pub fn qos(&self) -> &QosProfile {
        &self.inner.qos
    }

    /// The RTPS reader's GUID.
    #[must_use]
    pub fn guid(&self) -> Guid {
        self.inner.reader.guid()
    }

    /// The GID a ROS graph query sees.
    #[must_use]
    pub fn gid(&self) -> Gid {
        Gid::new(self.inner.context.compat(), self.guid())
    }

    /// The node that owns it in the ROS graph, if any.
    #[must_use]
    pub fn owner(&self) -> Option<&str> {
        self.inner.owner.as_deref()
    }

    /// The context it belongs to.
    #[must_use]
    pub fn context(&self) -> &Arc<Ros2Context> {
        &self.inner.context
    }

    /// Take the next sample, waiting for one.
    ///
    /// The wait is on a [`Notify`](tokio::sync::Notify) the participant's
    /// receive loop wakes, never on a timer.
    pub async fn recv(&self) -> (Vec<u8>, MessageInfo) {
        let sample = self.inner.reader.take().await;
        self.split(sample)
    }

    /// Take the next sample, or give up after `timeout`.
    pub async fn recv_within(&self, timeout: StdDuration) -> Option<(Vec<u8>, MessageInfo)> {
        self.inner
            .reader
            .take_within(timeout)
            .await
            .map(|sample| self.split(sample))
    }

    /// Take a sample if one is already waiting.
    pub async fn try_recv(&self) -> Option<(Vec<u8>, MessageInfo)> {
        self.inner
            .reader
            .try_take()
            .await
            .map(|sample| self.split(sample))
    }

    /// Take every sample waiting.
    pub async fn drain(&self) -> Vec<(Vec<u8>, MessageInfo)> {
        self.inner
            .reader
            .take_all()
            .await
            .into_iter()
            .map(|sample| self.split(sample))
            .collect()
    }

    /// How many samples are queued.
    pub async fn len(&self) -> usize {
        self.inner.reader.len().await
    }

    /// True when nothing is queued.
    pub async fn is_empty(&self) -> bool {
        self.inner.reader.is_empty().await
    }

    /// How many remote publications this subscription has matched.
    pub async fn publisher_count(&self) -> usize {
        self.inner
            .context
            .participant()
            .matched_writers(self.guid().entity_id)
            .await
    }

    /// Wait until at least `count` publications have matched.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Timeout`] when `timeout` elapses first.
    pub async fn wait_for_publishers(&self, count: usize, timeout: StdDuration) -> Ros2Result<()> {
        super::wait_for(
            &self.inner.context,
            timeout,
            "waiting for a publisher",
            || async { self.publisher_count().await >= count },
        )
        .await
    }

    /// Delete the subscription, announcing its disposal over SEDP.
    ///
    /// Samples already queued stay takeable; only new ones stop.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Rtps`] or [`Ros2Error::Cdr`].
    pub async fn destroy(&self) -> Ros2Result<bool> {
        if let (Some(node), Some(announcer)) =
            (self.inner.owner.as_deref(), self.inner.context.announcer())
        {
            announcer.remove_reader(node, self.guid()).await?;
        }
        self.inner.context.delete_reader(self.guid()).await
    }

    /// Split an RTPS sample into its payload and its info.
    fn split(&self, sample: Sample) -> (Vec<u8>, MessageInfo) {
        let info = MessageInfo::from_sample(&sample, self.inner.context.compat());
        (sample.payload, info)
    }
}

/// A subscription that decodes into one Rust message type.
#[derive(Debug)]
pub struct Subscription<T: MessageType> {
    raw: RawSubscription,
    marker: PhantomData<fn() -> T>,
}

impl<T: MessageType> Clone for Subscription<T> {
    fn clone(&self) -> Self {
        Self {
            raw: self.raw.clone(),
            marker: PhantomData,
        }
    }
}

impl<T: MessageType> Subscription<T> {
    /// Wrap a raw subscription whose type name is already `T`'s.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::DuplicateEntity`] when the type names disagree.
    pub fn from_raw(raw: RawSubscription) -> Ros2Result<Self> {
        if raw.type_name() != T::DDS_NAME {
            return Err(Ros2Error::DuplicateEntity {
                kind: "subscription",
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

    /// Create a typed subscription.
    ///
    /// # Errors
    ///
    /// As [`RawSubscription::owned_by`].
    pub async fn new(
        context: Arc<Ros2Context>,
        topic: FullName,
        qos: QosProfile,
        owner: Option<String>,
    ) -> Ros2Result<Self> {
        let raw = RawSubscription::owned_by(context, topic, T::DDS_NAME, qos, owner).await?;
        Ok(Self {
            raw,
            marker: PhantomData,
        })
    }

    /// The type-erased subscription underneath.
    #[must_use]
    pub const fn raw(&self) -> &RawSubscription {
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

    /// The RTPS reader's GUID.
    #[must_use]
    pub fn guid(&self) -> Guid {
        self.raw.guid()
    }

    /// The GID a ROS graph query sees.
    #[must_use]
    pub fn gid(&self) -> Gid {
        self.raw.gid()
    }

    /// Take and decode the next message, waiting for one.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Cdr`] when the octets are not a well-formed `T`. The
    /// sample is consumed either way — a peer that publishes the wrong type
    /// on a topic must not be able to wedge a subscription by making every
    /// subsequent `recv` re-read the same bad sample.
    pub async fn recv(&self) -> Ros2Result<(T, MessageInfo)> {
        let (payload, info) = self.raw.recv().await;
        Ok((astrs_cdr::from_bytes::<T>(&payload)?, info))
    }

    /// Take and decode the next message, or give up after `timeout`.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Timeout`] when nothing arrives, and [`Ros2Error::Cdr`]
    /// when what arrives will not decode.
    pub async fn recv_within(&self, timeout: StdDuration) -> Ros2Result<(T, MessageInfo)> {
        let Some((payload, info)) = self.raw.recv_within(timeout).await else {
            return Err(Ros2Error::Timeout {
                operation: "receiving a message",
                elapsed_ms: timeout.as_millis().min(u128::from(u64::MAX)) as u64,
            });
        };
        Ok((astrs_cdr::from_bytes::<T>(&payload)?, info))
    }

    /// Take and decode a message if one is already waiting.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Cdr`] when what is waiting will not decode.
    pub async fn try_recv(&self) -> Ros2Result<Option<(T, MessageInfo)>> {
        match self.raw.try_recv().await {
            None => Ok(None),
            Some((payload, info)) => Ok(Some((astrs_cdr::from_bytes::<T>(&payload)?, info))),
        }
    }

    /// How many samples are queued.
    pub async fn len(&self) -> usize {
        self.raw.len().await
    }

    /// True when nothing is queued.
    pub async fn is_empty(&self) -> bool {
        self.raw.is_empty().await
    }

    /// How many remote publications this subscription has matched.
    pub async fn publisher_count(&self) -> usize {
        self.raw.publisher_count().await
    }

    /// Wait until at least `count` publications have matched.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Timeout`].
    pub async fn wait_for_publishers(&self, count: usize, timeout: StdDuration) -> Ros2Result<()> {
        self.raw.wait_for_publishers(count, timeout).await
    }

    /// Delete the subscription.
    ///
    /// # Errors
    ///
    /// As [`RawSubscription::destroy`].
    pub async fn destroy(&self) -> Ros2Result<bool> {
        self.raw.destroy().await
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::msg::std_msgs;
    use crate::node::{ContextOptions, Ros2Context};
    use crate::pubsub::Publisher;

    async fn context() -> Arc<Ros2Context> {
        Ros2Context::new(ContextOptions::loopback())
            .await
            .expect("bind")
    }

    #[tokio::test]
    async fn a_typed_subscription_announces_the_mangled_names() {
        let context = context().await;
        let subscription = Subscription::<std_msgs::String>::new(
            Arc::clone(&context),
            FullName::topic("/chatter").expect("valid"),
            QosProfile::default(),
            None,
        )
        .await
        .expect("subscription");

        assert_eq!(subscription.topic().as_str(), "/chatter");
        assert_eq!(subscription.raw().dds_topic(), "rt/chatter");
        assert_eq!(
            subscription.raw().type_name(),
            "std_msgs::msg::dds_::String_"
        );
        assert!(subscription.is_empty().await);
        assert_eq!(subscription.publisher_count().await, 0);
        context.shutdown().await;
    }

    #[tokio::test]
    async fn receiving_with_nothing_queued_times_out() {
        let context = context().await;
        let subscription = Subscription::<std_msgs::String>::new(
            Arc::clone(&context),
            FullName::topic("/chatter").expect("valid"),
            QosProfile::default(),
            None,
        )
        .await
        .expect("subscription");
        let error = subscription
            .recv_within(StdDuration::from_millis(20))
            .await
            .expect_err("nothing published");
        assert!(matches!(error, Ros2Error::Timeout { .. }));
        assert!(subscription.try_recv().await.expect("no error").is_none());
        context.shutdown().await;
    }

    #[tokio::test]
    async fn a_publisher_and_subscription_in_one_participant_do_not_match_themselves() {
        // RTPS does not deliver a participant's own samples to its own
        // readers; the ROS graph shows both endpoints, but nothing crosses.
        // Asserting it here stops a later test from mistaking a self-loop for
        // a working exchange.
        let context = context().await;
        let publisher = Publisher::<std_msgs::String>::new(
            Arc::clone(&context),
            FullName::topic("/chatter").expect("valid"),
            QosProfile::default(),
            None,
        )
        .await
        .expect("publisher");
        let subscription = Subscription::<std_msgs::String>::new(
            Arc::clone(&context),
            FullName::topic("/chatter").expect("valid"),
            QosProfile::default(),
            None,
        )
        .await
        .expect("subscription");

        publisher
            .publish(&std_msgs::String {
                data: "self".to_owned(),
            })
            .await
            .expect("publish");
        assert!(
            subscription
                .recv_within(StdDuration::from_millis(50))
                .await
                .is_err()
        );
        context.shutdown().await;
    }

    #[tokio::test]
    async fn wrapping_a_raw_subscription_checks_the_type_name() {
        let context = context().await;
        let raw = RawSubscription::new(
            Arc::clone(&context),
            FullName::topic("/chatter").expect("valid"),
            "std_msgs::msg::dds_::Int32_",
            QosProfile::default(),
        )
        .await
        .expect("subscription");
        assert!(Subscription::<std_msgs::String>::from_raw(raw).is_err());
        context.shutdown().await;
    }

    #[tokio::test]
    async fn a_graph_owning_subscription_registers_and_unregisters() {
        let context = context().await;
        let subscription = Subscription::<std_msgs::String>::new(
            Arc::clone(&context),
            FullName::topic("/chatter").expect("valid"),
            QosProfile::default(),
            Some("/listener".to_owned()),
        )
        .await
        .expect("subscription");

        let announcer = context.announcer().expect("on");
        assert!(
            announcer
                .snapshot()
                .await
                .node("/listener")
                .expect("created")
                .owns(subscription.guid())
        );
        subscription.destroy().await.expect("destroy");
        assert!(
            !announcer
                .snapshot()
                .await
                .node("/listener")
                .expect("still there")
                .owns(subscription.guid())
        );
        context.shutdown().await;
    }

    #[test]
    fn message_info_reads_everything_the_sample_carries() {
        use astrs_rtps::behavior::{ChangeKind, InstanceHandle};
        use astrs_rtps::structure::{
            EntityId, EntityKind, GuidPrefix, SequenceNumber, Time, VendorId,
        };

        let writer = Guid::new(
            GuidPrefix::vendor_scoped(VendorId::ASTRS, [2; 10]),
            EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY),
        );
        let sample = Sample {
            writer,
            sequence_number: SequenceNumber::new(9),
            payload: vec![1, 2, 3, 4],
            source_timestamp: Some(Time::from_secs_nanos(17, 250_000_000)),
            received_at: Instant::now(),
            kind: ChangeKind::Alive,
            instance: InstanceHandle::NIL,
        };
        let info = MessageInfo::from_sample(&sample, RosCompat::Humble);
        assert_eq!(info.publisher, writer);
        assert_eq!(info.publisher_gid.len(), 24);
        assert_eq!(info.sequence_number, 9);
        assert!(info.is_alive);
        let stamp = info.source_timestamp.expect("a stamp");
        assert_eq!(stamp.sec, 17);
        assert!((stamp.nanosec as i64 - 250_000_000).abs() <= 1);
    }

    #[test]
    fn message_info_marks_a_disposal_as_not_alive() {
        use astrs_rtps::behavior::{ChangeKind, InstanceHandle};
        use astrs_rtps::structure::{EntityId, EntityKind, GuidPrefix, SequenceNumber, VendorId};

        let sample = Sample {
            writer: Guid::new(
                GuidPrefix::vendor_scoped(VendorId::ASTRS, [3; 10]),
                EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY),
            ),
            sequence_number: SequenceNumber::FIRST,
            payload: Vec::new(),
            source_timestamp: None,
            received_at: Instant::now(),
            kind: ChangeKind::NotAliveDisposed,
            instance: InstanceHandle::NIL,
        };
        let info = MessageInfo::from_sample(&sample, RosCompat::Jazzy);
        assert!(!info.is_alive);
        assert_eq!(info.source_timestamp, None);
        assert_eq!(info.publisher_gid.len(), 16);
    }
}
