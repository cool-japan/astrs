//! [`Ros2Context`]: one RTPS participant, and everything shared between the
//! nodes that live on it.
//!
//! ROS 2 does not put one node in one DDS participant. A component
//! container hosts a dozen nodes in one process and one participant, and
//! `ros_discovery_info` exists precisely because DDS discovery alone cannot
//! tell them apart. This crate follows that: a [`Ros2Context`] owns the
//! participant, the clock and the graph announcer, and a
//! [`Ros2Node`](crate::node::Ros2Node) is a name plus a set of endpoints on
//! it.
//!
//! # What the background task does
//!
//! [`Ros2Context::new`] spawns the participant's run loop, which is what
//! sends SPDP announcements, answers heartbeats, expires leases and
//! delivers received samples into their subscriptions. Nothing in this
//! crate polls; a subscription's `recv` awaits a
//! [`Notify`](tokio::sync::Notify) the run loop wakes.
//!
//! [`Ros2Context::shutdown`] announces the participant's departure — a
//! disposal on the SPDP topic — so peers unwire immediately rather than
//! after a hundred-second lease. It is idempotent, and dropping the last
//! clone without calling it is legal but leaves peers waiting.
//!
//! # Discovery on a sandboxed host
//!
//! [`ContextOptions::loopback`](crate::node::ContextOptions::loopback) plus
//! [`with_peer`](crate::node::ContextOptions::with_peer) is the
//! deterministic path, and the one this crate's tests use: no multicast
//! join, ephemeral ports, and the peer's metatraffic locator handed over
//! directly. [`Ros2Context::multicast`] reports what the kernel actually
//! said rather than hiding it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use astrs_rtps::behavior::{Participant, ParticipantConfig, ReaderHandle, TopicKey, WriterHandle};
use astrs_rtps::discovery::{
    DiscoveryDb, DiscoveryEvent, Gid, RosCompat, SpdpConfig, WriterQos as RtpsWriterQos,
};
use astrs_rtps::structure::{Guid, Locator};
use tokio::sync::{Mutex, broadcast};
use tokio::task::JoinHandle;

use crate::error::{Ros2Error, Ros2Result};
use crate::graph::announcer::GraphAnnouncer;
use crate::node::options::ContextOptions;
use crate::qos::QosProfile;
use crate::time::Ros2Clock;

/// One RTPS participant and the state every node on it shares.
///
/// Cloneable through the [`Arc`] every constructor returns; every clone is
/// the same context.
#[derive(Debug)]
pub struct Ros2Context {
    participant: Participant,
    clock: Ros2Clock,
    compat: RosCompat,
    domain_id: u32,
    announcer: Option<GraphAnnouncer>,
    local: Mutex<BTreeMap<Guid, LocalEndpoint>>,
    task: Mutex<Option<JoinHandle<Result<(), astrs_rtps::BehaviorError>>>>,
}

/// One endpoint this participant created.
///
/// The RTPS discovery database holds only *remote* endpoints — a
/// participant does not announce to itself — so a graph query that asked
/// only the database would show every node in the system except this one.
/// The context therefore records what it creates, and
/// [`crate::graph::GraphCache`] merges the two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalEndpoint {
    /// The DDS topic name, already mangled.
    pub dds_topic: String,
    /// The DDS type name.
    pub dds_type: String,
    /// True for a publication, false for a subscription.
    pub is_writer: bool,
}

impl Ros2Context {
    /// Bind a participant and start its background loop.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Rtps`] when a socket will not bind, when the domain or
    /// participant id is out of range, or when the graph announcer's writer
    /// cannot be created.
    pub async fn new(options: ContextOptions) -> Ros2Result<Arc<Self>> {
        let spdp = SpdpConfig::new(
            options.domain_id,
            options.participant_id,
            options.resolved_guid_prefix(),
        )
        .map_err(Ros2Error::Rtps)?
        .with_multicast(options.multicast)
        .with_initial_peers(options.initial_peers.clone())
        .with_compat(options.compat);
        let spdp = SpdpConfig {
            user_data: options.user_data(),
            ..spdp
        };

        let config = ParticipantConfig::from_spdp(spdp)
            .with_bind(options.bind)
            .with_tick_period(options.tick_period);
        let participant = Participant::new(config).await.map_err(Ros2Error::Rtps)?;

        let announcer = if options.announce_graph {
            Some(GraphAnnouncer::new(&participant, options.compat).await?)
        } else {
            None
        };

        let context = Arc::new(Self {
            clock: Ros2Clock::new(),
            compat: options.compat,
            domain_id: options.domain_id,
            announcer,
            local: Mutex::new(BTreeMap::new()),
            task: Mutex::new(None),
            participant,
        });
        let handle = context.participant.spawn(options.tick_period);
        *context.task.lock().await = Some(handle);
        Ok(context)
    }

    /// A context on the deterministic loopback path, announcing to `peers`.
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new).
    pub async fn loopback(peers: impl IntoIterator<Item = Locator>) -> Ros2Result<Arc<Self>> {
        let mut options = ContextOptions::loopback();
        options.initial_peers.extend(peers);
        Self::new(options).await
    }

    /// The RTPS participant underneath.
    ///
    /// The escape hatch a layering test needs — and the reason this crate
    /// can be proved to sit *on* `astrs-rtps` rather than beside it.
    #[must_use]
    pub const fn participant(&self) -> &Participant {
        &self.participant
    }

    /// The clock every node on this context reads.
    #[must_use]
    pub const fn clock(&self) -> &Ros2Clock {
        &self.clock
    }

    /// Which distribution's conventions this context speaks.
    #[must_use]
    pub const fn compat(&self) -> RosCompat {
        self.compat
    }

    /// The DDS domain.
    #[must_use]
    pub const fn domain_id(&self) -> u32 {
        self.domain_id
    }

    /// This participant's GUID.
    #[must_use]
    pub fn guid(&self) -> Guid {
        self.participant.guid()
    }

    /// This participant's GID, at the configured width.
    #[must_use]
    pub fn gid(&self) -> Gid {
        Gid::new(self.compat, self.participant.guid())
    }

    /// The locator a peer should be handed as an initial peer.
    #[must_use]
    pub fn metatraffic_locator(&self) -> Locator {
        self.participant.metatraffic_locator()
    }

    /// What the kernel said when the multicast join was attempted.
    #[must_use]
    pub fn multicast(&self) -> &astrs_rtps::behavior::MulticastCapability {
        self.participant.multicast()
    }

    /// Subscribe to RTPS discovery events.
    #[must_use]
    pub fn discovery_events(&self) -> broadcast::Receiver<DiscoveryEvent> {
        self.participant.events()
    }

    /// A snapshot of everything discovery knows.
    pub async fn discovery(&self) -> DiscoveryDb {
        self.participant.discovery_snapshot().await
    }

    /// The graph announcer, when `ros_discovery_info` is enabled.
    #[must_use]
    pub const fn announcer(&self) -> Option<&GraphAnnouncer> {
        self.announcer.as_ref()
    }

    /// Create a publication on an already-mangled DDS topic name.
    ///
    /// The layer beneath [`RawPublisher`](crate::pubsub::RawPublisher):
    /// names are mangled by the node, and everything below here speaks DDS.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Rtps`] when the names are unusable or the SEDP
    /// announcement will not encode.
    pub async fn create_writer(
        &self,
        dds_topic: &str,
        dds_type: &str,
        qos: &QosProfile,
    ) -> Ros2Result<WriterHandle> {
        let key = TopicKey::new(dds_topic, dds_type).map_err(Ros2Error::Rtps)?;
        let writer = self
            .participant
            .create_writer(key, qos.to_writer_qos())
            .await
            .map_err(Ros2Error::Rtps)?;
        self.record_local(writer.guid(), dds_topic, dds_type, true)
            .await;
        Ok(writer)
    }

    /// Create a subscription on an already-mangled DDS topic name.
    ///
    /// # Errors
    ///
    /// As [`create_writer`](Self::create_writer).
    pub async fn create_reader(
        &self,
        dds_topic: &str,
        dds_type: &str,
        qos: &QosProfile,
    ) -> Ros2Result<ReaderHandle> {
        let key = TopicKey::new(dds_topic, dds_type).map_err(Ros2Error::Rtps)?;
        let reader = self
            .participant
            .create_reader(key, qos.to_reader_qos())
            .await
            .map_err(Ros2Error::Rtps)?;
        self.record_local(reader.guid(), dds_topic, dds_type, false)
            .await;
        Ok(reader)
    }

    /// Create a publication with an RTPS QoS value directly.
    ///
    /// What the graph announcer needs: `ros_discovery_info` is announced
    /// with a QoS profile no ROS-level API is meant to be able to express.
    ///
    /// # Errors
    ///
    /// As [`create_writer`](Self::create_writer).
    pub async fn create_writer_with_rtps_qos(
        &self,
        dds_topic: &str,
        dds_type: &str,
        qos: RtpsWriterQos,
    ) -> Ros2Result<WriterHandle> {
        let key = TopicKey::new(dds_topic, dds_type).map_err(Ros2Error::Rtps)?;
        let writer = self
            .participant
            .create_writer(key, qos)
            .await
            .map_err(Ros2Error::Rtps)?;
        self.record_local(writer.guid(), dds_topic, dds_type, true)
            .await;
        Ok(writer)
    }

    /// Delete a publication, announcing its disposal.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Rtps`].
    pub async fn delete_writer(&self, writer: Guid) -> Ros2Result<bool> {
        self.local.lock().await.remove(&writer);
        self.participant
            .delete_writer(writer)
            .await
            .map_err(Ros2Error::Rtps)
    }

    /// Delete a subscription, announcing its disposal.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Rtps`].
    pub async fn delete_reader(&self, reader: Guid) -> Ros2Result<bool> {
        self.local.lock().await.remove(&reader);
        self.participant
            .delete_reader(reader)
            .await
            .map_err(Ros2Error::Rtps)
    }

    /// Every endpoint this participant has created and not deleted.
    pub async fn local_endpoints(&self) -> BTreeMap<Guid, LocalEndpoint> {
        self.local.lock().await.clone()
    }

    /// Record one locally-created endpoint.
    async fn record_local(&self, guid: Guid, dds_topic: &str, dds_type: &str, is_writer: bool) {
        self.local.lock().await.insert(
            guid,
            LocalEndpoint {
                dds_topic: dds_topic.to_owned(),
                dds_type: dds_type.to_owned(),
                is_writer,
            },
        );
    }

    /// Announce this participant now, whatever the cadence says.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Rtps`].
    pub async fn announce(&self) -> Ros2Result<usize> {
        self.participant.announce().await.map_err(Ros2Error::Rtps)
    }

    /// Run one cadence round by hand.
    ///
    /// The background loop does this on a timer; a test that wants to make
    /// progress deterministically calls it directly.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Rtps`].
    pub async fn tick(&self) -> Ros2Result<()> {
        self.participant.tick().await.map_err(Ros2Error::Rtps)
    }

    /// True once [`shutdown`](Self::shutdown) has run.
    pub async fn is_shut_down(&self) -> bool {
        self.participant.is_shut_down().await
    }

    /// Wait until `peer` has been discovered, or give up.
    ///
    /// Waits on the discovery event stream rather than polling, so the only
    /// timing in it is the caller's own deadline.
    ///
    /// # Errors
    ///
    /// [`Ros2Error::Timeout`] when `timeout` elapses first.
    pub async fn wait_for_participant(&self, peer: Guid, timeout: StdDuration) -> Ros2Result<()> {
        let mut events = self.discovery_events();
        if self.participant.knows(peer).await {
            return Ok(());
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(Ros2Error::Timeout {
                    operation: "discovering a participant",
                    elapsed_ms: timeout.as_millis().min(u128::from(u64::MAX)) as u64,
                });
            }
            match tokio::time::timeout(remaining, events.recv()).await {
                Ok(Ok(event)) => {
                    if event.guid().participant_guid() == peer.participant_guid()
                        && event.is_arrival()
                    {
                        return Ok(());
                    }
                }
                // A lagged receiver has missed events; the database is the
                // authority, so re-check it rather than giving up.
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                    if self.participant.knows(peer).await {
                        return Ok(());
                    }
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => {
                    return Err(Ros2Error::NodeShutDown);
                }
                Err(_) => {
                    return Err(Ros2Error::Timeout {
                        operation: "discovering a participant",
                        elapsed_ms: timeout.as_millis().min(u128::from(u64::MAX)) as u64,
                    });
                }
            }
            if self.participant.knows(peer).await {
                return Ok(());
            }
        }
    }

    /// Announce departure and stop the background loop.
    ///
    /// Idempotent.
    pub async fn shutdown(&self) {
        self.participant.shutdown().await;
        if let Some(handle) = self.task.lock().await.take() {
            // The run loop returns on its own once `shutdown` sets the flag;
            // awaiting it means a test can assert the participant is really
            // gone rather than racing it.
            let _ = handle.await;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::node::options::ContextOptions;

    async fn pair() -> (Arc<Ros2Context>, Arc<Ros2Context>) {
        let first = Ros2Context::new(ContextOptions::loopback())
            .await
            .expect("bind");
        let second = Ros2Context::loopback([first.metatraffic_locator()])
            .await
            .expect("bind");
        (first, second)
    }

    #[tokio::test]
    async fn a_context_binds_ephemeral_ports_and_reports_them() {
        let context = Ros2Context::new(ContextOptions::loopback())
            .await
            .expect("bind");
        assert_ne!(context.metatraffic_locator().udp_port(), Some(0));
        assert_eq!(context.domain_id(), 0);
        assert_eq!(context.compat(), RosCompat::Jazzy);
        assert_eq!(context.gid().guid(), Some(context.guid()));
        context.shutdown().await;
    }

    #[tokio::test]
    async fn multicast_is_reported_rather_than_assumed() {
        let context = Ros2Context::new(ContextOptions::loopback())
            .await
            .expect("bind");
        assert_eq!(
            context.multicast(),
            &astrs_rtps::behavior::MulticastCapability::Disabled,
            "the loopback preset turns it off; the capability says so"
        );
        context.shutdown().await;
    }

    #[tokio::test]
    async fn two_contexts_discover_each_other_over_unicast() {
        let (first, second) = pair().await;
        second
            .wait_for_participant(first.guid(), StdDuration::from_secs(5))
            .await
            .expect("the peer announces to its initial peer");
        first
            .wait_for_participant(second.guid(), StdDuration::from_secs(5))
            .await
            .expect("and the announcement is answered");
        first.shutdown().await;
        second.shutdown().await;
    }

    #[tokio::test]
    async fn waiting_for_an_unknown_participant_times_out() {
        let context = Ros2Context::new(ContextOptions::loopback())
            .await
            .expect("bind");
        let stranger = astrs_rtps::structure::Guid::new(
            astrs_rtps::structure::GuidPrefix::vendor_scoped(
                astrs_rtps::structure::VendorId::ASTRS,
                [9; 10],
            ),
            astrs_rtps::structure::ENTITYID_PARTICIPANT,
        );
        let error = context
            .wait_for_participant(stranger, StdDuration::from_millis(50))
            .await
            .expect_err("nobody is there");
        assert!(matches!(error, Ros2Error::Timeout { .. }));
        assert!(error.is_transient());
        context.shutdown().await;
    }

    #[tokio::test]
    async fn shutting_down_twice_is_harmless() {
        let context = Ros2Context::new(ContextOptions::loopback())
            .await
            .expect("bind");
        context.shutdown().await;
        context.shutdown().await;
        assert!(context.is_shut_down().await);
    }

    #[tokio::test]
    async fn the_graph_announcer_is_on_by_default_and_can_be_turned_off() {
        let announcing = Ros2Context::new(ContextOptions::loopback())
            .await
            .expect("bind");
        assert!(announcing.announcer().is_some());
        announcing.shutdown().await;

        let quiet = Ros2Context::new(ContextOptions::loopback().with_graph_announcement(false))
            .await
            .expect("bind");
        assert!(quiet.announcer().is_none());
        quiet.shutdown().await;
    }

    #[tokio::test]
    async fn writers_and_readers_take_mangled_names() {
        let context = Ros2Context::new(ContextOptions::loopback())
            .await
            .expect("bind");
        let writer = context
            .create_writer(
                "rt/chatter",
                "std_msgs::msg::dds_::String_",
                &QosProfile::default(),
            )
            .await
            .expect("writer");
        let reader = context
            .create_reader(
                "rt/chatter",
                "std_msgs::msg::dds_::String_",
                &QosProfile::default(),
            )
            .await
            .expect("reader");
        assert_eq!(writer.topic().topic_name, "rt/chatter");
        assert_eq!(reader.topic().type_name, "std_msgs::msg::dds_::String_");

        assert!(context.delete_writer(writer.guid()).await.expect("delete"));
        assert!(context.delete_reader(reader.guid()).await.expect("delete"));
        context.shutdown().await;
    }

    #[tokio::test]
    async fn an_empty_topic_name_is_refused_by_the_layer_below() {
        let context = Ros2Context::new(ContextOptions::loopback())
            .await
            .expect("bind");
        let error = context
            .create_writer("", "T", &QosProfile::default())
            .await
            .expect_err("empty");
        assert!(matches!(error, Ros2Error::Rtps(_)));
        context.shutdown().await;
    }

    #[tokio::test]
    async fn the_clock_is_shared_by_everything_on_the_context() {
        let context = Ros2Context::new(ContextOptions::loopback())
            .await
            .expect("bind");
        context.clock().set_sim_time(true);
        context.clock().feed_clock(crate::time::RosTime::new(3, 0));
        assert_eq!(context.clock().now(), crate::time::RosTime::new(3, 0));
        context.shutdown().await;
    }

    #[tokio::test]
    async fn a_ticked_context_makes_progress_without_the_loop() {
        let context = Ros2Context::new(ContextOptions::loopback())
            .await
            .expect("bind");
        context.tick().await.expect("tick");
        assert_eq!(context.announce().await.expect("announce"), 0, "no peers");
        context.shutdown().await;
    }

    #[tokio::test]
    async fn the_enclave_reaches_the_spdp_announcement() {
        let context = Ros2Context::new(ContextOptions::loopback().with_enclave("/robot"))
            .await
            .expect("bind");
        let peer = Ros2Context::loopback([context.metatraffic_locator()])
            .await
            .expect("bind");
        peer.wait_for_participant(context.guid(), StdDuration::from_secs(5))
            .await
            .expect("discovered");
        let db = peer.discovery().await;
        let remote = db.participant(context.guid()).expect("known");
        assert_eq!(remote.data.user_data, b"enclave=/robot;");
        context.shutdown().await;
        peer.shutdown().await;
    }
}
