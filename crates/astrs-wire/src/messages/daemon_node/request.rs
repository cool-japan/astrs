//! `node → daemon`: [`NodeRequest`] (blueprint §7.3, §24.1).
//!
//! Everything a node asks of its local daemon: let me in, give me my inputs,
//! deliver this output, I am finished with that one, hold this handle for me,
//! yes — I have switched to the shared-memory plane, and my own deadline
//! monitor just measured a violation.
//!
//! Frozen variant indices 0–10, plus one tail append:
//! [`NodeRequest::ReportDeadlineViolation`] (§11.3) — nothing in §24.1
//! anticipated a node reporting its *own* measured condition back to the
//! daemon for relay, since every other verb either asks the daemon to do
//! something or answers a daemon-initiated question.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{FrameKind, NodeRequest, OutputPayload, WireMessage};
//!
//! let send = NodeRequest::SendMessage {
//!     output: "image".parse()?,
//!     metadata: Default::default(),
//!     payload: OutputPayload::inline(vec![0; 128]),
//! };
//! assert_eq!(NodeRequest::KIND, FrameKind::NodeRequest);
//! assert_eq!(send.variant_index(), 2);
//! assert_eq!(send.payload_len(), 128);
//! # Ok::<(), astrs_wire::IdError>(())
//! ```

use core::fmt;

use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::common::duration::DurationMs;
use crate::frame::FrameKind;
use crate::ids::DataId;
use crate::messages::daemon_node::types::{ExtensionKey, NodeHandshake, OutputPayload};
use crate::messages::impl_wire_message;
use crate::metadata::Metadata;

/// The default number of events a single [`NodeRequest::NextEvent`] may drain.
pub const DEFAULT_EVENT_BATCH: u32 = 32;

/// The node → daemon message family (§24.1).
///
/// # Examples
///
/// ```
/// use astrs_wire::{NodeRequest, WireMessage};
///
/// let next = NodeRequest::NextEvent { timeout: None, max_batch: 1 };
/// assert_eq!(next.variant_name(), "NextEvent");
/// assert!(!next.is_send());
/// ```
// No `Eq`: `SendMessage` carries [`Metadata`], whose parameters may hold an
// `f64`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum NodeRequest {
    /// Attach to the daemon (§8.3, §24.2 `ASTRS_NODE_CONFIG`).
    ///
    /// Answered by [`crate::NodeEvent::Registered`].
    #[oxicode(variant = 0)]
    Register(NodeHandshake),
    /// Begin receiving inputs.
    ///
    /// Sent once the node is ready to handle events; until then the daemon
    /// queues them under each input's queue policy (§11.2) rather than
    /// delivering into a process that is still starting up.
    #[oxicode(variant = 1)]
    Subscribe {
        /// The inputs to subscribe to; empty means every declared input.
        inputs: Vec<DataId>,
    },
    /// Publish one message on an output.
    #[oxicode(variant = 2)]
    SendMessage {
        /// The output to publish on.
        output: DataId,
        /// The metadata riding beside the payload (§6.1).
        metadata: Metadata,
        /// The payload, inline or as a shared-memory slot reference.
        payload: OutputPayload,
    },
    /// This output will produce nothing further.
    #[oxicode(variant = 3)]
    OutputDone {
        /// The output that is finished.
        output: DataId,
    },
    /// Close several outputs at once — what a node does as it exits.
    #[oxicode(variant = 4)]
    CloseOutputs {
        /// The outputs to close; empty means every output the node has.
        outputs: Vec<DataId>,
    },
    /// Ask for the next event(s) on the node's event stream.
    ///
    /// The push and pull styles coexist: a node that simply reads its stream
    /// never sends this, while one with its own event loop uses it to control
    /// when work arrives.
    #[oxicode(variant = 5)]
    NextEvent {
        /// Return an empty batch after this long rather than blocking forever.
        timeout: Option<DurationMs>,
        /// The most events to return in one answer.
        max_batch: u32,
    },
    /// The node dropped its event stream and wants no more events.
    ///
    /// The daemon stops queueing for it immediately — dora's lesson: a node
    /// that stopped reading must not make the daemon grow a queue for it
    /// forever.
    #[oxicode(variant = 6)]
    EventStreamDropped,
    /// Store a value in the daemon's extension table.
    #[oxicode(variant = 7)]
    ExtStore {
        /// The key to store under.
        key: ExtensionKey,
        /// The bytes to store.
        value: Vec<u8>,
        /// Drop the entry after this long, so a crashed node's handles do not
        /// accumulate.
        ttl: Option<DurationMs>,
    },
    /// Read a value back out of the extension table.
    ///
    /// Answered by [`crate::NodeEvent::ExtValue`].
    #[oxicode(variant = 8)]
    ExtLoad {
        /// The key to read.
        key: ExtensionKey,
    },
    /// Drop an extension entry.
    #[oxicode(variant = 9)]
    ExtDrop {
        /// The key to drop.
        key: ExtensionKey,
    },
    /// Acknowledge a [`crate::NodeEvent::RouteUpgrade`] (§6.3).
    ///
    /// The daemon must know that the producer has actually switched before it
    /// stops brokering the route, or messages would fall between the two
    /// planes.
    #[oxicode(variant = 10)]
    RouteUpgradeAck {
        /// The output whose route was upgraded.
        output: DataId,
        /// Whether the node accepted the upgrade. A node that cannot map the
        /// segment answers `false` and stays on the daemon path.
        accepted: bool,
        /// Why it refused, when it refused.
        reason: Option<String>,
    },
    /// A per-input latency budget this node's own `astrs_scheduler::DeadlineMonitor`
    /// measured was exceeded (§11.3) — a tail append beyond §24.1.
    ///
    /// Only the node that owns the input can detect this: the daemon sees
    /// each input's arrival and each output's publish independently, with no
    /// way to know which publish was that node's *answer* to which input
    /// (the node's own event-loop ordering is what supplies that link, per
    /// `astrs_scheduler::DeadlineMonitor`'s module docs — this crate does not
    /// depend on `astrs-scheduler`, so this is prose, not a link). So the
    /// measurement happens node-side, in that monitor,
    /// and this is how the result reaches the daemon — which relays it onto
    /// `astrs/status` as [`crate::NodeEvent::DeadlineViolated`] and counts it,
    /// exactly as it already does for a peer's `NodeFailed`/`Restarted`.
    #[oxicode(variant = 11)]
    ReportDeadlineViolation {
        /// The input whose input-to-output latency budget was exceeded.
        input: DataId,
        /// The budget it was held to.
        budget: DurationMs,
        /// The measured latency.
        latency: DurationMs,
    },
}

impl NodeRequest {
    /// Whether this request publishes a message.
    #[must_use]
    pub const fn is_send(&self) -> bool {
        matches!(self, Self::SendMessage { .. })
    }

    /// The output this request concerns, when it concerns one.
    #[must_use]
    pub const fn output(&self) -> Option<&DataId> {
        match self {
            Self::SendMessage { output, .. }
            | Self::OutputDone { output }
            | Self::RouteUpgradeAck { output, .. } => Some(output),
            _ => None,
        }
    }

    /// The extension key this request addresses, when it addresses one.
    #[must_use]
    pub const fn extension_key(&self) -> Option<&ExtensionKey> {
        match self {
            Self::ExtStore { key, .. } | Self::ExtLoad { key } | Self::ExtDrop { key } => Some(key),
            _ => None,
        }
    }

    /// The payload bytes this request carries, for bandwidth accounting.
    ///
    /// A shared-memory reference counts as the bytes it points at, since that
    /// is the data the route is moving even though the frame is tiny.
    #[must_use]
    pub fn payload_len(&self) -> u64 {
        match self {
            Self::SendMessage { payload, .. } => payload.len(),
            Self::ExtStore { value, .. } => value.len() as u64,
            _ => 0,
        }
    }

    /// Whether this request uses the zero-copy plane.
    #[must_use]
    pub const fn is_zero_copy(&self) -> bool {
        match self {
            Self::SendMessage { payload, .. } => payload.is_zero_copy(),
            _ => false,
        }
    }

    /// Whether this request ends the node's participation in the dataflow.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::CloseOutputs { .. } | Self::EventStreamDropped)
    }

    /// The registration this request carries, if it is one.
    #[must_use]
    pub const fn handshake(&self) -> Option<&NodeHandshake> {
        match self {
            Self::Register(handshake) => Some(handshake),
            _ => None,
        }
    }

    /// Compares two requests with `f64` bit patterns rather than IEEE
    /// equality — see [`crate::PeerEvent::bitwise_eq`].
    #[must_use]
    pub fn bitwise_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::SendMessage {
                    output: left_output,
                    metadata: left_metadata,
                    payload: left_payload,
                },
                Self::SendMessage {
                    output: right_output,
                    metadata: right_metadata,
                    payload: right_payload,
                },
            ) => {
                left_output == right_output
                    && left_metadata.bitwise_eq(right_metadata)
                    && left_payload == right_payload
            }
            _ => self == other,
        }
    }
}

impl fmt::Display for NodeRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Register(handshake) => write!(f, "register {handshake}"),
            Self::Subscribe { inputs } => {
                if inputs.is_empty() {
                    f.write_str("subscribe to every input")
                } else {
                    write!(f, "subscribe to {} input(s)", inputs.len())
                }
            }
            Self::SendMessage {
                output, payload, ..
            } => write!(f, "send {payload} on {output}"),
            Self::OutputDone { output } => write!(f, "output {output} done"),
            Self::CloseOutputs { outputs } => {
                if outputs.is_empty() {
                    f.write_str("close every output")
                } else {
                    write!(f, "close {} output(s)", outputs.len())
                }
            }
            Self::NextEvent { max_batch, .. } => write!(f, "next event (≤{max_batch})"),
            Self::EventStreamDropped => f.write_str("event stream dropped"),
            Self::ExtStore { key, value, .. } => {
                write!(f, "store {} byte(s) at {key}", value.len())
            }
            Self::ExtLoad { key } => write!(f, "load {key}"),
            Self::ExtDrop { key } => write!(f, "drop {key}"),
            Self::RouteUpgradeAck {
                output, accepted, ..
            } => write!(
                f,
                "route upgrade of {output} {}",
                if *accepted { "accepted" } else { "refused" }
            ),
            Self::ReportDeadlineViolation {
                input,
                budget,
                latency,
            } => write!(
                f,
                "deadline violated on {input}: {}ms over a {}ms budget",
                latency.as_millis().saturating_sub(budget.as_millis()),
                budget.as_millis(),
            ),
        }
    }
}

impl_wire_message!(
    NodeRequest,
    FrameKind::NodeRequest,
    [
        "Register",
        "Subscribe",
        "SendMessage",
        "OutputDone",
        "CloseOutputs",
        "NextEvent",
        "EventStreamDropped",
        "ExtStore",
        "ExtLoad",
        "ExtDrop",
        "RouteUpgradeAck",
        "ReportDeadlineViolation",
    ],
    fn variant_index(&self) -> u16 {
        match self {
            Self::Register(_) => 0,
            Self::Subscribe { .. } => 1,
            Self::SendMessage { .. } => 2,
            Self::OutputDone { .. } => 3,
            Self::CloseOutputs { .. } => 4,
            Self::NextEvent { .. } => 5,
            Self::EventStreamDropped => 6,
            Self::ExtStore { .. } => 7,
            Self::ExtLoad { .. } => 8,
            Self::ExtDrop { .. } => 9,
            Self::RouteUpgradeAck { .. } => 10,
            Self::ReportDeadlineViolation { .. } => 11,
        }
    }
);

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_time::HlcTimestamp;

    use super::*;
    use crate::codec::{WireDecode, WireEncode, round_trip};
    use crate::frame::{FrameFlags, FrameLimits};
    use crate::messages::WireMessage;
    use crate::messages::daemon_node::types::ExtensionNamespace;
    use crate::messages::samples::node_requests;

    #[test]
    fn the_family_has_the_eleven_frozen_variants_plus_one_tail_append() {
        assert_eq!(NodeRequest::VARIANT_NAMES.len(), 12);
        assert_eq!(NodeRequest::VARIANT_NAMES[0], "Register");
        assert_eq!(NodeRequest::VARIANT_NAMES[10], "RouteUpgradeAck");
        assert_eq!(NodeRequest::VARIANT_NAMES[11], "ReportDeadlineViolation");
    }

    #[test]
    fn every_variant_reports_and_encodes_its_frozen_index() {
        let samples = node_requests().unwrap();
        assert_eq!(samples.len(), NodeRequest::VARIANT_NAMES.len());
        for (index, sample) in samples.iter().enumerate() {
            let index = u16::try_from(index).unwrap();
            assert_eq!(sample.variant_index(), index, "{sample:?}");
            assert_eq!(u16::from(sample.encode_to_vec().unwrap()[0]), index);
        }
    }

    #[test]
    fn every_variant_round_trips_through_a_frame() {
        let limits = FrameLimits::uds();
        for sample in node_requests().unwrap() {
            let bytes = sample.to_frame(FrameFlags::EMPTY, &limits).unwrap();
            let decoded = NodeRequest::from_bytes(&bytes, &limits).unwrap();
            assert!(decoded.bitwise_eq(&sample), "{sample:?}");
        }
    }

    #[test]
    fn trailing_bytes_after_a_request_are_refused() {
        for sample in node_requests().unwrap() {
            let mut bytes = sample.encode_to_vec().unwrap();
            bytes.push(7);
            assert!(matches!(
                NodeRequest::decode_exact(&bytes),
                Err(crate::error::WireError::TrailingBytes { .. })
            ));
        }
    }

    #[test]
    fn a_send_reports_its_payload_on_both_planes() {
        let inline = NodeRequest::SendMessage {
            output: DataId::new("image").unwrap(),
            metadata: Metadata::new(HlcTimestamp::new(1, 0)),
            payload: OutputPayload::inline(vec![0; 100]),
        };
        assert!(inline.is_send());
        assert_eq!(inline.payload_len(), 100);
        assert!(!inline.is_zero_copy());
        assert_eq!(inline.output().map(DataId::as_str), Some("image"));

        let zero_copy = NodeRequest::SendMessage {
            output: DataId::new("image").unwrap(),
            metadata: Metadata::new(HlcTimestamp::new(1, 0)),
            payload: OutputPayload::Shm {
                segment: "astrs/df/camera/3/image".to_owned(),
                slot: 2,
                len: 4 << 20,
                generation: 3,
            },
        };
        assert!(zero_copy.is_zero_copy());
        assert_eq!(
            zero_copy.payload_len(),
            4 << 20,
            "a slot reference still moves its bytes"
        );
    }

    #[test]
    fn extension_verbs_expose_their_key() {
        let key = ExtensionKey::user("calibration").unwrap();
        for request in [
            NodeRequest::ExtStore {
                key: key.clone(),
                value: vec![1, 2],
                ttl: Some(DurationMs::from_secs(60)),
            },
            NodeRequest::ExtLoad { key: key.clone() },
            NodeRequest::ExtDrop { key: key.clone() },
        ] {
            assert_eq!(request.extension_key(), Some(&key));
            assert_eq!(round_trip(&request).unwrap(), request);
        }
        assert_eq!(NodeRequest::EventStreamDropped.extension_key(), None);

        let reserved = ExtensionKey::new(ExtensionNamespace::PinnedMemory, "pool").unwrap();
        assert!(!reserved.is_writable_by_node());
    }

    #[test]
    fn terminal_requests_are_the_two_that_end_participation() {
        let terminal: Vec<&'static str> = node_requests()
            .unwrap()
            .iter()
            .filter(|request| request.is_terminal())
            .map(NodeRequest::variant_name)
            .collect();
        assert_eq!(terminal, vec!["CloseOutputs", "EventStreamDropped"]);
    }

    #[test]
    fn a_registration_is_recognised() {
        let sample = node_requests()
            .unwrap()
            .into_iter()
            .find(NodeRequest::is_send)
            .expect("the sample table covers SendMessage");
        assert!(sample.handshake().is_none());

        let handshake = NodeHandshake::new(
            crate::ids::DataflowId::from_u128(1),
            crate::ids::NodeId::new("camera").unwrap(),
            2,
        );
        let request = NodeRequest::Register(handshake.clone());
        assert_eq!(request.handshake(), Some(&handshake));
    }

    #[test]
    fn nan_metadata_survives_the_wire() {
        let mut metadata = Metadata::new(HlcTimestamp::new(1, 0));
        metadata.insert("nan", f64::NAN).unwrap();
        let request = NodeRequest::SendMessage {
            output: DataId::new("image").unwrap(),
            metadata,
            payload: OutputPayload::empty(),
        };
        let decoded = round_trip(&request).unwrap();
        assert_ne!(decoded, request);
        assert!(decoded.bitwise_eq(&request));
    }

    #[test]
    fn display_names_every_variant_without_panicking() {
        for sample in node_requests().unwrap() {
            assert!(!sample.to_string().is_empty());
        }
        assert!(
            NodeRequest::Subscribe { inputs: Vec::new() }
                .to_string()
                .contains("every input")
        );
        assert!(
            NodeRequest::CloseOutputs {
                outputs: Vec::new()
            }
            .to_string()
            .contains("every output")
        );
    }

    #[test]
    fn the_default_event_batch_is_documented() {
        assert_eq!(DEFAULT_EVENT_BATCH, 32);
    }
}
