//! One channel of the verification model: an edge, its queue, and what the
//! blueprint's queue semantics mean for it.

use astrs_graph::{Edge, EdgeKey, EdgeSource, NodeId, PortName, QueueConfig};
use astrs_manifest::{QueuePolicy, VirtualSource, recognize_virtual_source};

use crate::scale::{Nanos, Rate};

/// The factor by which the `backpressure` queue policy over-buffers before
/// it starts dropping (blueprint §11.2: "backpressure buffers to 10×, then
/// drops with an ERROR log + metric").
pub const BACKPRESSURE_OVERBUFFER: u64 = 10;

/// What produces a channel's messages.
///
/// Distinguishing the three cases is what lets the deadlock encoding treat
/// a timer edge as *spontaneously* markable — a place produced by a
/// transition with no input places, which is exactly why a timer-fed
/// channel can never belong to a starving siphon (see
/// [`crate::obligations::deadlock`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Producer {
    /// An ordinary `node/output` producer.
    Node {
        /// The producing node.
        node: NodeId,
        /// The output port on that node.
        output: PortName,
    },
    /// A periodic virtual timer (`astrs/timer/…`), with its exact rate.
    Timer {
        /// The verbatim source string, for reports.
        source: String,
        /// The exact declared rate.
        rate: Rate,
    },
    /// An aperiodic virtual source (`astrs/logs…`, `astrs/status`): the
    /// daemon produces on it spontaneously, but at no declared rate.
    Aperiodic {
        /// The verbatim source string, for reports.
        source: String,
    },
}

impl Producer {
    /// The producing node, when this channel is fed by one.
    #[must_use]
    pub fn node(&self) -> Option<&NodeId> {
        match self {
            Self::Node { node, .. } => Some(node),
            Self::Timer { .. } | Self::Aperiodic { .. } => None,
        }
    }

    /// Whether messages appear on this channel without any graph node
    /// having to fire first.
    #[must_use]
    pub fn is_spontaneous(&self) -> bool {
        matches!(self, Self::Timer { .. } | Self::Aperiodic { .. })
    }

    /// The declared rate, for a periodic timer.
    #[must_use]
    pub fn declared_rate(&self) -> Option<Rate> {
        match self {
            Self::Timer { rate, .. } => Some(*rate),
            Self::Node { .. } | Self::Aperiodic { .. } => None,
        }
    }

    /// How this producer is written in a report.
    #[must_use]
    pub fn render(&self) -> String {
        match self {
            Self::Node { node, output } => format!("{node}/{output}"),
            Self::Timer { source, .. } | Self::Aperiodic { source } => source.clone(),
        }
    }
}

/// One channel: the queue behind a single wired input.
#[derive(Debug, Clone, PartialEq)]
pub struct Channel {
    /// This channel's identity — the consumer `(node, input)` pair.
    pub key: EdgeKey,
    /// What feeds it.
    pub producer: Producer,
    /// The `queue_size` the manifest declared.
    pub declared_capacity: u64,
    /// The declared queue policy.
    pub policy: QueuePolicy,
    /// The number of messages that can actually sit in this queue before
    /// one is dropped.
    ///
    /// For `drop_oldest` this is the declared `queue_size`. For
    /// `backpressure` it is [`BACKPRESSURE_OVERBUFFER`] times that, per
    /// blueprint §11.2 — the over-buffer is the *real* bound a latency
    /// argument has to reason with, because a message really can be
    /// queued behind that many others before anything is dropped.
    pub effective_capacity: u64,
    /// The declared per-input delivery timeout, if any.
    ///
    /// This is the one place the manifest itself states a timing
    /// expectation about an edge (blueprint §8.3's long-form input), and
    /// so it is the manifest-native source of both a
    /// [`rate`](crate::obligations::rate) obligation ("messages must
    /// arrive at least this often") and a
    /// [`latency`](crate::obligations::latency) obligation ("and must
    /// arrive within this long of being produced").
    pub timeout: Option<Nanos>,
    /// Whether this channel carries correlated (service/action) traffic,
    /// which blueprint §11.2 grants eviction immunity.
    pub correlated: bool,
}

impl Channel {
    /// Build a channel model from a graph edge.
    ///
    /// # Errors
    ///
    /// Returns [`crate::VerifyError::BadDuration`] if the edge's declared
    /// `timeout` is not representable on the integer nanosecond scale.
    pub fn from_edge(key: &EdgeKey, edge: &Edge, correlated: bool) -> crate::Result<Self> {
        let producer = producer_of(&edge.from);
        let QueueConfig {
            size,
            policy,
            timeout,
        } = &edge.queue;
        let declared_capacity = u64::from(*size);
        let effective_capacity = match policy {
            QueuePolicy::DropOldest => declared_capacity,
            QueuePolicy::Backpressure => declared_capacity.saturating_mul(BACKPRESSURE_OVERBUFFER),
        };
        let timeout = timeout
            .map(|value| Nanos::from_manifest(format!("{key}.timeout"), value))
            .transpose()?;
        Ok(Self {
            key: key.clone(),
            producer,
            declared_capacity,
            policy: *policy,
            effective_capacity,
            timeout,
            correlated,
        })
    }

    /// The consuming node.
    #[must_use]
    pub fn consumer(&self) -> &NodeId {
        &self.key.consumer
    }

    /// The consuming input port.
    #[must_use]
    pub fn input(&self) -> &PortName {
        &self.key.input
    }

    /// Whether this channel can drop messages at all.
    ///
    /// Correlated traffic is eviction-immune (blueprint §11.2), so a
    /// service request or a goal status is never evicted to make room —
    /// dropping one would wedge the peer forever, which is the very
    /// deadlock [`crate::obligations::deadlock`] exists to rule out.
    #[must_use]
    pub fn can_drop(&self) -> bool {
        !self.correlated
    }

    /// How this channel is written in a report: `consumer.input ← producer`.
    #[must_use]
    pub fn render(&self) -> String {
        format!("{} <- {}", self.key, self.producer.render())
    }
}

/// Classify an edge source into a [`Producer`].
fn producer_of(source: &EdgeSource) -> Producer {
    match source {
        EdgeSource::NodeOutput { node, output } => Producer::Node {
            node: node.clone(),
            output: output.clone(),
        },
        EdgeSource::Virtual(text) => match recognize_virtual_source(text) {
            Some(Ok(parsed)) => match Rate::from_virtual_source(&parsed) {
                Some(rate) => Producer::Timer {
                    source: text.clone(),
                    rate,
                },
                None => Producer::Aperiodic {
                    source: text.clone(),
                },
            },
            // An unrecognized `astrs/...` source cannot reach here from a
            // validated manifest (`Manifest::validate` rejects it), and a
            // string without the prefix is not a virtual source at all.
            // Either way the safe model is "something outside the graph
            // produces on this channel at no declared rate", which is what
            // `Aperiodic` says — never "nothing produces on it", which
            // would let the deadlock encoding claim a starvation that the
            // graph does not exhibit.
            _ => Producer::Aperiodic {
                source: text.clone(),
            },
        },
    }
}

/// The parsed virtual source behind a channel, when it has one.
///
/// Exposed for reports that want the structured form rather than the raw
/// string.
#[must_use]
pub fn virtual_source_of(channel: &Channel) -> Option<VirtualSource> {
    let text = match &channel.producer {
        Producer::Timer { source, .. } | Producer::Aperiodic { source } => source,
        Producer::Node { .. } => return None,
    };
    recognize_virtual_source(text)?.ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use astrs_manifest::DurationSecs;

    fn key(consumer: &str, input: &str) -> EdgeKey {
        EdgeKey::new(NodeId::new(consumer), PortName::new(input))
    }

    fn edge(from: EdgeSource, size: u32, policy: QueuePolicy, timeout: Option<f64>) -> Edge {
        Edge {
            from,
            queue: QueueConfig {
                size,
                policy,
                timeout: timeout.map(|secs| {
                    DurationSecs::from_secs_f64(secs).expect("test durations are finite")
                }),
            },
        }
    }

    #[test]
    fn node_sourced_channel_keeps_its_producer() {
        let source = EdgeSource::NodeOutput {
            node: NodeId::new("camera"),
            output: PortName::new("frames"),
        };
        let channel = Channel::from_edge(
            &key("detector", "frames"),
            &edge(source, 4, QueuePolicy::DropOldest, None),
            false,
        )
        .expect("representable");
        assert_eq!(channel.producer.node(), Some(&NodeId::new("camera")));
        assert!(!channel.producer.is_spontaneous());
        assert_eq!(channel.producer.render(), "camera/frames");
        assert_eq!(channel.render(), "detector.frames <- camera/frames");
    }

    #[test]
    fn timer_sourced_channel_carries_its_exact_rate() {
        let channel = Channel::from_edge(
            &key("planner", "tick"),
            &edge(
                EdgeSource::Virtual("astrs/timer/hz/50".to_string()),
                1,
                QueuePolicy::DropOldest,
                None,
            ),
            false,
        )
        .expect("representable");
        assert!(channel.producer.is_spontaneous());
        assert_eq!(channel.producer.declared_rate(), Rate::new(50, 1));
    }

    #[test]
    fn aperiodic_virtual_sources_have_no_rate_but_stay_spontaneous() {
        let channel = Channel::from_edge(
            &key("supervisor", "status"),
            &edge(
                EdgeSource::Virtual("astrs/status".to_string()),
                8,
                QueuePolicy::DropOldest,
                None,
            ),
            false,
        )
        .expect("representable");
        assert!(channel.producer.is_spontaneous());
        assert_eq!(channel.producer.declared_rate(), None);
    }

    #[test]
    fn unrecognized_virtual_sources_stay_spontaneous() {
        let channel = Channel::from_edge(
            &key("n", "i"),
            &edge(
                EdgeSource::Virtual("astrs/nope/1".to_string()),
                1,
                QueuePolicy::DropOldest,
                None,
            ),
            false,
        )
        .expect("representable");
        assert!(
            channel.producer.is_spontaneous(),
            "an unmodellable source must not be mistaken for a starved one"
        );
        assert_eq!(virtual_source_of(&channel), None);
    }

    #[test]
    fn backpressure_multiplies_the_effective_capacity() {
        let channel = Channel::from_edge(
            &key("sink", "data"),
            &edge(
                EdgeSource::Virtual("astrs/timer/hz/1".to_string()),
                3,
                QueuePolicy::Backpressure,
                None,
            ),
            false,
        )
        .expect("representable");
        assert_eq!(channel.declared_capacity, 3);
        assert_eq!(channel.effective_capacity, 30);
    }

    #[test]
    fn drop_oldest_keeps_the_declared_capacity() {
        let channel = Channel::from_edge(
            &key("sink", "data"),
            &edge(
                EdgeSource::Virtual("astrs/timer/hz/1".to_string()),
                1,
                QueuePolicy::DropOldest,
                None,
            ),
            false,
        )
        .expect("representable");
        assert_eq!(channel.effective_capacity, 1);
    }

    #[test]
    fn correlated_channels_cannot_drop() {
        let channel = Channel::from_edge(
            &key("server", "request"),
            &edge(
                EdgeSource::NodeOutput {
                    node: NodeId::new("client"),
                    output: PortName::new("request"),
                },
                2,
                QueuePolicy::DropOldest,
                None,
            ),
            true,
        )
        .expect("representable");
        assert!(channel.correlated);
        assert!(!channel.can_drop());
    }

    #[test]
    fn timeouts_convert_to_nanoseconds() {
        let channel = Channel::from_edge(
            &key("detector", "frames"),
            &edge(
                EdgeSource::Virtual("astrs/timer/hz/10".to_string()),
                2,
                QueuePolicy::DropOldest,
                Some(0.25),
            ),
            false,
        )
        .expect("representable");
        assert_eq!(channel.timeout, Some(Nanos::new(250_000_000)));
    }

    #[test]
    fn virtual_source_of_parses_the_structured_form() {
        let channel = Channel::from_edge(
            &key("planner", "tick"),
            &edge(
                EdgeSource::Virtual("astrs/timer/millis/40".to_string()),
                1,
                QueuePolicy::DropOldest,
                None,
            ),
            false,
        )
        .expect("representable");
        assert_eq!(
            virtual_source_of(&channel),
            Some(VirtualSource::TimerMillis(40))
        );
    }

    #[test]
    fn consumer_and_input_come_from_the_key() {
        let channel = Channel::from_edge(
            &key("planner", "tick"),
            &edge(
                EdgeSource::Virtual("astrs/timer/hz/1".to_string()),
                1,
                QueuePolicy::DropOldest,
                None,
            ),
            false,
        )
        .expect("representable");
        assert_eq!(channel.consumer(), &NodeId::new("planner"));
        assert_eq!(channel.input(), &PortName::new("tick"));
    }
}
