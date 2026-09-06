//! Topic endpoints: one DDS subscription or publication per bridged topic.
//!
//! # The two directions
//!
//! ```text
//!   to_astrs    DDS sample ─▶ codec.decode ─▶ RawOutput::send_batch
//!   from_astrs  Event::Input ─▶ codec.encode ─▶ RawPublisher::publish_bytes_at
//! ```
//!
//! A `to_astrs` endpoint is *driven by the network*, so it lives on its own
//! tokio task with a channel back to [`crate::run()`]'s loop; a `from_astrs`
//! endpoint is driven by the node's event stream and needs no task of its
//! own. That asymmetry is why [`InboundTopic`] holds a
//! [`astrs_node_api::RawOutput`] (which is `&mut`-only, and therefore stays
//! in the loop) while [`OutboundTopic`] holds a [`RawPublisher`] (which is
//! `Clone` and `Send`).
//!
//! # Which instant a bridged sample carries
//!
//! Three candidates, in this order (§10.5, §14):
//!
//! 1. `header.stamp`, when the message type has a `std_msgs/Header` and the
//!    stamp is not zero — the instant the *sensor* observed the world.
//! 2. The `INFO_TS` source timestamp the publisher sent, when it sent one —
//!    the instant the *publisher* wrote the sample.
//! 3. This bridge's own HLC reading — the instant the sample was received.
//!
//! Each is strictly worse than the one above it as a description of when
//! the data is *about*, and each is strictly better than nothing. The
//! chosen instant becomes the message's HLC
//! [`Metadata`] timestamp, so a `.arec` recording (§14) replays the graph
//! against sensor time rather than against bridge time.
//!
//! In the other direction the graph's own HLC becomes the sample's DDS
//! source timestamp, through
//! [`RawPublisher::publish_bytes_at`] — a bridged message keeps its
//! provenance in both directions rather than being re-dated on the way out.

use std::sync::Arc;

use astrs_node_api::{Node, Payload, RawOutput};
use astrs_ros2::node::Ros2Node;
use astrs_ros2::pubsub::{MessageInfo, RawPublisher, RawSubscription};
use astrs_ros2::time::RosTime;
use astrs_wire::Metadata;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;

use crate::error::BridgeResult;
use crate::plan::TopicBridge;
use crate::resolve::TypeBinding;

/// The metadata key naming the ROS 2 topic a bridged sample came from.
///
/// Set only on an opaque (non-columnar) sample: a columnar payload already
/// carries its schema fingerprint, and stamping two extra strings onto every
/// sample of a high-rate topic is exactly the needless per-message
/// allocation §3 rules out.
pub const META_ROS_TOPIC: &str = "ros_topic";

/// The metadata key naming the ROS 2 type an opaque sample carries.
pub const META_ROS_TYPE: &str = "ros_type";

/// One `direction: to_astrs` endpoint: DDS in, AstRS out.
#[derive(Debug)]
pub struct InboundTopic {
    /// What the plan said.
    pub bridge: TopicBridge,
    /// How the type is carried.
    pub binding: TypeBinding,
    /// The AstRS port to publish on.
    pub output: RawOutput,
}

/// One `direction: from_astrs` endpoint: AstRS in, DDS out.
#[derive(Debug)]
pub struct OutboundTopic {
    /// What the plan said.
    pub bridge: TopicBridge,
    /// How the type is carried.
    pub binding: TypeBinding,
    /// The DDS publication.
    pub publisher: RawPublisher,
}

/// Which of the bridge's DDS subscriptions a sample arrived on.
///
/// One flat enum for all three endpoint families, because [`spawn_pump`]
/// and [`crate::run()`]'s loop are the same code for every one of them: a
/// subscription, a channel, and a match at the far end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum EndpointSlot {
    /// A `direction: to_astrs` topic, by index into the inbound list.
    Topic(usize),
    /// A service's `rq/` request half, by index into the service list.
    ServiceRequest(usize),
    /// A service's `rr/` reply half.
    ServiceReply(usize),
    /// An action's `send_goal` request half, by index into the action list.
    ActionGoalRequest(usize),
    /// An action's `send_goal` reply half.
    ActionGoalReply(usize),
    /// An action's `cancel_goal` request half.
    ActionCancelRequest(usize),
    /// An action's `cancel_goal` reply half.
    ActionCancelReply(usize),
    /// An action's `get_result` request half.
    ActionResultRequest(usize),
    /// An action's `get_result` reply half.
    ActionResultReply(usize),
    /// An action's `feedback` topic.
    ActionFeedback(usize),
    /// An action's `status` topic.
    ActionStatus(usize),
}

/// A sample that arrived on one of the bridge's DDS subscriptions.
#[derive(Debug)]
pub struct InboundSample {
    /// Which subscription it arrived on.
    pub slot: EndpointSlot,
    /// The CDR octets, encapsulation header included.
    pub payload: Vec<u8>,
    /// What RTPS knows about it.
    pub info: MessageInfo,
}

/// Create the DDS subscription for a `to_astrs` topic, and the AstRS output
/// beside it.
///
/// # Errors
///
/// [`crate::BridgeError::Ros2`] when the subscription cannot be created (a name the
/// participant refuses, a socket failure), and [`crate::BridgeError::Node`] when
/// the node has no such output — which [`crate::plan()`] has already ruled
/// out, so it means the daemon and the manifest disagree.
pub async fn open_inbound(
    node: &mut Node,
    ros: &Ros2Node,
    bridge: TopicBridge,
    binding: TypeBinding,
) -> BridgeResult<(InboundTopic, RawSubscription)> {
    let subscription = ros
        .create_raw_subscription(&bridge.topic, binding.dds_type_name(), bridge.qos)
        .await?;
    let output = node.raw_output(bridge.port.as_str())?;
    Ok((
        InboundTopic {
            bridge,
            binding,
            output,
        },
        subscription,
    ))
}

/// Create the DDS publication for a `from_astrs` topic.
///
/// # Errors
///
/// [`crate::BridgeError::Ros2`] when the publication cannot be created.
pub async fn open_outbound(
    ros: &Ros2Node,
    bridge: TopicBridge,
    binding: TypeBinding,
) -> BridgeResult<OutboundTopic> {
    let publisher = ros
        .create_raw_publisher(&bridge.topic, binding.dds_type_name(), bridge.qos)
        .await?;
    Ok(OutboundTopic {
        bridge,
        binding,
        publisher,
    })
}

/// Pump one subscription into `sink` until the channel closes.
///
/// One task per `to_astrs` topic. It owns the subscription and does nothing
/// but forward, so a slow conversion in the main loop cannot stall the RTPS
/// reader's own cadence; the channel's bound is what applies backpressure.
#[must_use]
pub fn spawn_pump(
    slot: EndpointSlot,
    subscription: Arc<RawSubscription>,
    sink: Sender<InboundSample>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let (payload, info) = subscription.recv().await;
            let sample = InboundSample {
                slot,
                payload,
                info,
            };
            if sink.send(sample).await.is_err() {
                // The loop is gone: shutdown, not an error.
                break;
            }
        }
    })
}

/// Convert one DDS sample and publish it on the AstRS side.
///
/// # Errors
///
/// [`crate::BridgeError::Node`] when the output will not accept the message.
/// A sample that does not *decode* is not an error: it is logged and
/// dropped, because one malformed sample from one peer must not take a
/// whole bridge down (§12's fault containment). The `Ok(false)` return says
/// that happened.
pub fn forward_inbound(
    node: &Node,
    topic: &mut InboundTopic,
    sample: &InboundSample,
) -> BridgeResult<bool> {
    let stamp_hint = sample.info.source_timestamp;

    match &topic.binding {
        TypeBinding::Generated(codec) => {
            let decoded = match codec.decode(&sample.payload) {
                Ok(decoded) => decoded,
                Err(error) => {
                    tracing::warn!(
                        topic = %topic.bridge.topic,
                        %error,
                        "dropping an undecodable ROS 2 sample"
                    );
                    return Ok(false);
                }
            };
            let meta = inbound_metadata(node, decoded.stamp.or(stamp_hint));
            topic.output.send_batch(&decoded.batch, meta)?;
        }
        TypeBinding::Discovered(found) => {
            let mut meta = inbound_metadata(node, stamp_hint);
            annotate_opaque(&mut meta, &topic.bridge.topic, &found.ros_type_name);
            topic.output.send_bytes(&sample.payload, meta)?;
        }
    }
    Ok(true)
}

/// Convert one AstRS message and publish it on the ROS 2 side.
///
/// # Errors
///
/// [`crate::BridgeError::Ros2`] when the DDS write fails. As with
/// [`forward_inbound`], a message that will not *convert* is logged and
/// dropped rather than fatal, and reported as `Ok(false)`.
pub async fn forward_outbound(
    topic: &OutboundTopic,
    meta: &Metadata,
    payload: &Payload,
) -> BridgeResult<bool> {
    let stamp = RosTime::from_hlc(meta.timestamp);

    let octets = match &topic.binding {
        TypeBinding::Generated(codec) => {
            let batch = match payload.batch() {
                Ok(batch) => batch,
                Err(error) => {
                    tracing::warn!(
                        topic = %topic.bridge.topic,
                        %error,
                        "dropping an AstRS message that is not a columnar batch"
                    );
                    return Ok(false);
                }
            };
            match codec.encode(batch) {
                Ok(octets) => octets,
                Err(error) => {
                    tracing::warn!(
                        topic = %topic.bridge.topic,
                        %error,
                        "dropping an AstRS message that will not encode as CDR"
                    );
                    return Ok(false);
                }
            }
        }
        // An opaque topic carries the CDR octets themselves, so the
        // outbound path is a copy rather than a conversion.
        TypeBinding::Discovered(_) => payload.to_vec(),
    };

    topic.publisher.publish_bytes_at(octets, stamp).await?;
    Ok(true)
}

/// The metadata a bridged inbound sample carries.
///
/// See the module docs for the three-candidate stamp rule. A ROS stamp
/// before the Unix epoch cannot become an (unsigned) HLC timestamp, so it
/// falls through to the bridge's own reading rather than wrapping.
#[must_use]
pub fn inbound_metadata(node: &Node, stamp: Option<RosTime>) -> Metadata {
    stamp
        .and_then(RosTime::to_hlc)
        .map_or_else(|| node.metadata(), Metadata::new)
}

/// Stamp an opaque sample with the ROS identity a consumer needs to decode
/// it.
pub fn annotate_opaque(meta: &mut Metadata, topic: &str, ros_type: &str) {
    // A reserved or malformed key is impossible here — both are literals
    // that pass `[A-Za-z0-9_.-]+` — but `insert` is fallible, so the
    // failure is dropped rather than unwrapped.
    let _ = meta.insert(META_ROS_TOPIC, topic.to_owned());
    let _ = meta.insert(META_ROS_TYPE, ros_type.to_owned());
}

/// Every inbound endpoint's subscription task, joined on shutdown.
#[derive(Debug, Default)]
pub struct Pumps {
    handles: Vec<JoinHandle<()>>,
}

impl Pumps {
    /// An empty set.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            handles: Vec::new(),
        }
    }

    /// Take ownership of one more task.
    pub fn push(&mut self, handle: JoinHandle<()>) {
        self.handles.push(handle);
    }

    /// How many tasks are running.
    #[must_use]
    pub fn len(&self) -> usize {
        self.handles.len()
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }

    /// Abort every task and wait for it to unwind.
    ///
    /// Abort rather than a shutdown flag: every task is parked in
    /// `subscription.recv()`, which has no cancellation token of its own,
    /// and the subscription it holds is dropped by the abort — which is
    /// exactly the "no lingering DDS participants" §12 asks for.
    pub async fn shutdown(&mut self) {
        for handle in &self.handles {
            handle.abort();
        }
        for handle in self.handles.drain(..) {
            // A cancelled task reports `JoinError::Cancelled`, which is the
            // expected outcome rather than a failure.
            let _ = handle.await;
        }
    }
}

/// The subscriptions a bridge holds, kept alive for as long as it runs.
///
/// A [`RawSubscription`] deletes its RTPS reader when the last clone drops,
/// so this type is what keeps the readers matched; dropping it is what
/// unmatches them.
#[derive(Debug, Default)]
pub struct Subscriptions(Vec<Arc<RawSubscription>>);

impl Subscriptions {
    /// An empty set.
    #[must_use]
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    /// Keep one more subscription alive.
    pub fn push(&mut self, subscription: Arc<RawSubscription>) {
        self.0.push(subscription);
    }

    /// How many are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Destroy every reader, so the participant announces their departure
    /// before the process exits.
    pub async fn shutdown(&mut self) {
        for subscription in self.0.drain(..) {
            if let Err(error) = subscription.destroy().await {
                tracing::debug!(%error, "a subscription was already gone at shutdown");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_time::HlcTimestamp;

    use super::*;

    #[test]
    fn a_positive_ros_stamp_becomes_the_hlc_timestamp() {
        let stamp = RosTime::new(1_700_000_000, 250_000_000);
        let hlc = stamp.to_hlc().unwrap();
        assert_eq!(hlc.physical_ns(), 1_700_000_000_250_000_000);
        assert_eq!(hlc.logical(), 0);
        assert_eq!(Metadata::new(hlc).timestamp, hlc);
    }

    #[test]
    fn a_pre_epoch_ros_stamp_has_no_hlc_form() {
        assert_eq!(RosTime::new(-1, 0).to_hlc(), None);
    }

    #[test]
    fn the_outbound_stamp_is_the_messages_own_hlc() {
        let hlc = HlcTimestamp::new(1_700_000_000_250_000_000, 3);
        let stamp = RosTime::from_hlc(hlc);
        assert_eq!(stamp, RosTime::new(1_700_000_000, 250_000_000));
    }

    #[test]
    fn an_opaque_sample_is_annotated_with_its_ros_identity() {
        let mut meta = Metadata::new(HlcTimestamp::new(1, 0));
        annotate_opaque(&mut meta, "/scan", "my_msgs/msg/Custom");
        assert_eq!(
            meta.get(META_ROS_TOPIC)
                .and_then(astrs_wire::Parameter::as_str),
            Some("/scan")
        );
        assert_eq!(
            meta.get(META_ROS_TYPE)
                .and_then(astrs_wire::Parameter::as_str),
            Some("my_msgs/msg/Custom")
        );
    }

    #[tokio::test]
    async fn an_empty_pump_set_shuts_down_cleanly() {
        let mut pumps = Pumps::new();
        assert!(pumps.is_empty());
        pumps.shutdown().await;

        let mut subscriptions = Subscriptions::new();
        assert!(subscriptions.is_empty());
        subscriptions.shutdown().await;
    }

    #[tokio::test]
    async fn a_pump_stops_when_its_sink_closes() {
        // No participant is needed to prove the shutdown contract: an
        // aborted task is joined rather than leaked, which is what
        // `Pumps::shutdown` promises.
        let handle = tokio::spawn(async {
            std::future::pending::<()>().await;
        });
        let mut pumps = Pumps::new();
        pumps.push(handle);
        assert_eq!(pumps.len(), 1);
        pumps.shutdown().await;
        assert!(pumps.is_empty());
    }
}
