//! `daemon ↔ daemon`: [`PeerEvent`] (blueprint §7.3, §24.1).
//!
//! Two daemons on different hosts talk to each other for exactly one reason:
//! carrying a route whose producer sits on one machine and whose consumer sits
//! on the other. The family is therefore small and symmetric — the same enum
//! travels in both directions, because either side may open a route, either
//! side may tear one down, and either side may ping.
//!
//! ```text
//! producer daemon                         consumer daemon
//!        │ RouteSetup { route_id, spec }        │
//!        ├─────────────────────────────────────►│
//!        │        RouteAccept { acceptance }    │
//!        │◄─────────────────────────────────────┤
//!        │ Output { route_id, seq, payload }    │
//!        ├─────────────────────────────────────►│
//!        │ OutputClosed { route_id, reason }    │
//!        ├─────────────────────────────────────►│
//!        │ RouteTeardown { route_id, reason }   │
//!        │◄────────────────────────────────────►│
//! ```
//!
//! # Why a [`RouteId`] and not a [`crate::RouteKey`]
//!
//! The key that names a route — dataflow UUID plus two `node/port` strings —
//! costs upwards of sixty bytes. A 30 Hz camera on a 10-node graph would spend
//! megabytes a minute restating it. The two daemons therefore agree on a
//! connection-scoped [`RouteId`] during setup and every payload frame carries
//! that instead (§3, minimal per-message overhead).
//!
//! # Payload opacity
//!
//! [`PeerEvent::Output`] carries **raw bytes**. The payload is an Arrow IPC
//! stream (§6.1), but `astrs-wire` does not depend on `astrs-data`: a relaying
//! daemon must be able to forward a payload it cannot interpret, and the wire
//! crate must stay at the bottom of the dependency graph.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{
//!     Compression, FrameFlags, FrameKind, FrameLimits, PeerEvent, Plane, RouteAcceptance,
//!     RouteId, WireMessage,
//! };
//!
//! let accept = PeerEvent::RouteAccept {
//!     route_id: RouteId::FIRST,
//!     acceptance: RouteAcceptance::accepted(Plane::Quic, Compression::Zstd, 1 << 20),
//! };
//!
//! assert_eq!(PeerEvent::KIND, FrameKind::PeerEvent);
//! assert_eq!(accept.variant_index(), 1);
//!
//! let limits = FrameLimits::network();
//! let bytes = accept.to_frame(FrameFlags::CRC, &limits)?;
//! assert_eq!(PeerEvent::from_bytes(&bytes, &limits)?, accept);
//! # Ok::<(), astrs_wire::WireError>(())
//! ```

use core::fmt;

use astrs_time::HlcTimestamp;
use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::common::route::{RouteAcceptance, RouteCloseReason, RouteSpec};
use crate::frame::FrameKind;
use crate::ids::{RouteId, TypeUrn};
use crate::messages::impl_wire_message;
use crate::metadata::Metadata;

/// The daemon ↔ daemon message family (§24.1).
///
/// Frozen variant indices: `RouteSetup = 0`, `RouteAccept = 1`,
/// `RouteTeardown = 2`, `Output = 3`, `OutputClosed = 4`, `Ping = 5`.
///
/// # Examples
///
/// ```
/// use astrs_wire::{PeerEvent, RouteId, WireMessage};
///
/// let output = PeerEvent::Output {
///     route_id: RouteId::new(4),
///     seq: 17,
///     metadata: Default::default(),
///     payload: vec![0xAA; 8],
/// };
/// assert_eq!(output.variant_name(), "Output");
/// assert!(output.is_payload());
/// assert_eq!(output.route_id(), Some(RouteId::new(4)));
/// ```
// No `Eq`: `Output` carries [`Metadata`], whose parameters may hold an `f64`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PeerEvent {
    /// Open a route from this daemon's producer to the peer's consumer.
    ///
    /// Sent by the daemon that owns the **producer**. The handle is minted by
    /// the sender and is unique on this connection.
    #[oxicode(variant = 0)]
    RouteSetup {
        /// The handle both sides will use from now on.
        route_id: RouteId,
        /// The full route description: key, plane, compression, queue policy.
        route: RouteSpec,
        /// The producer incarnation this route belongs to. A route from a
        /// stale generation is refused rather than silently attached (§6.2).
        generation: u64,
        /// The producer's declared port type, `None` for `type: any` (§3.7).
        type_urn: Option<TypeUrn>,
        /// The largest payload the producer intends to send, in bytes, so the
        /// acceptor can size buffers before the first frame arrives.
        max_payload_bytes: u64,
        /// Bytes of receive-side pool the producer would like reserved, if it
        /// has an opinion. Advisory: the acceptor answers with what it will
        /// actually honour in [`RouteAcceptance::Accepted`].
        pool_hint_bytes: Option<u64>,
    },
    /// The answer to a [`PeerEvent::RouteSetup`].
    #[oxicode(variant = 1)]
    RouteAccept {
        /// The handle from the setup this answers.
        route_id: RouteId,
        /// Accepted (with terms) or rejected (with a cause).
        acceptance: RouteAcceptance,
    },
    /// Close a route in either direction.
    ///
    /// Unlike [`PeerEvent::OutputClosed`], which says "this output produced its
    /// last message", a teardown says "this delivery path is gone" — the
    /// dataflow stopped, the consumer died, the transport failed.
    #[oxicode(variant = 2)]
    RouteTeardown {
        /// The route being torn down.
        route_id: RouteId,
        /// Why.
        reason: RouteCloseReason,
    },
    /// One payload for an established route.
    ///
    /// This is the hot path: everything else in this family happens once per
    /// route, this happens once per message.
    #[oxicode(variant = 3)]
    Output {
        /// The route the payload belongs to.
        route_id: RouteId,
        /// A per-route sequence number, starting at zero, so the consumer can
        /// detect a gap left by a dropped datagram.
        seq: u64,
        /// The metadata riding beside the payload (§6.1).
        metadata: Metadata,
        /// The payload bytes — an Arrow IPC stream, opaque here.
        payload: Vec<u8>,
    },
    /// The producer will send nothing more on this route.
    ///
    /// The route stays open until a [`PeerEvent::RouteTeardown`] so that the
    /// consumer can drain what is already in flight.
    #[oxicode(variant = 4)]
    OutputClosed {
        /// The route whose producer finished.
        route_id: RouteId,
        /// The sequence number of the last message sent, so the consumer knows
        /// when it has drained everything.
        final_seq: u64,
        /// Why the output closed.
        reason: RouteCloseReason,
    },
    /// Liveness probe and round-trip measurement.
    ///
    /// A peer answers a `Ping` with `is_reply: true` and the *same* nonce and
    /// `sent_at`, so the originator measures a true round trip without keeping
    /// per-nonce state.
    #[oxicode(variant = 5)]
    Ping {
        /// An opaque value echoed back unchanged.
        nonce: u64,
        /// When the originator sent the probe.
        sent_at: HlcTimestamp,
        /// `false` on the probe, `true` on the echo.
        is_reply: bool,
    },
}

impl PeerEvent {
    /// A probe with the given nonce.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_time::HlcTimestamp;
    /// use astrs_wire::PeerEvent;
    ///
    /// let probe = PeerEvent::ping(1, HlcTimestamp::new(10, 0));
    /// let echo = probe.pong().expect("a probe has an echo");
    /// assert!(matches!(echo, PeerEvent::Ping { is_reply: true, .. }));
    /// ```
    #[must_use]
    pub const fn ping(nonce: u64, sent_at: HlcTimestamp) -> Self {
        Self::Ping {
            nonce,
            sent_at,
            is_reply: false,
        }
    }

    /// The echo for this probe, or `None` if this is not a probe.
    ///
    /// Answering an echo with another echo would loop forever, so a value that
    /// already has `is_reply: true` returns `None`.
    #[must_use]
    pub const fn pong(&self) -> Option<Self> {
        match self {
            Self::Ping {
                nonce,
                sent_at,
                is_reply: false,
            } => Some(Self::Ping {
                nonce: *nonce,
                sent_at: *sent_at,
                is_reply: true,
            }),
            _ => None,
        }
    }

    /// The route this event concerns, if it concerns one.
    ///
    /// [`PeerEvent::Ping`] is the only variant that does not.
    #[must_use]
    pub const fn route_id(&self) -> Option<RouteId> {
        match self {
            Self::RouteSetup { route_id, .. }
            | Self::RouteAccept { route_id, .. }
            | Self::RouteTeardown { route_id, .. }
            | Self::Output { route_id, .. }
            | Self::OutputClosed { route_id, .. } => Some(*route_id),
            Self::Ping { .. } => None,
        }
    }

    /// Whether this event carries bulk payload rather than route control.
    ///
    /// Transports use it to pick a stream: payload goes on the route's own
    /// unidirectional stream, control stays on stream 0 (§6.4).
    #[must_use]
    pub const fn is_payload(&self) -> bool {
        matches!(self, Self::Output { .. })
    }

    /// The payload bytes, if this is an [`PeerEvent::Output`].
    #[must_use]
    pub fn payload(&self) -> Option<&[u8]> {
        match self {
            Self::Output { payload, .. } => Some(payload),
            _ => None,
        }
    }

    /// The number of payload bytes this event carries (zero for control
    /// events), for bandwidth accounting.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{PeerEvent, RouteId};
    ///
    /// let output = PeerEvent::Output {
    ///     route_id: RouteId::FIRST,
    ///     seq: 0,
    ///     metadata: Default::default(),
    ///     payload: vec![0; 128],
    /// };
    /// assert_eq!(output.payload_len(), 128);
    /// assert_eq!(PeerEvent::ping(0, Default::default()).payload_len(), 0);
    /// ```
    #[must_use]
    pub fn payload_len(&self) -> usize {
        self.payload().map_or(0, <[u8]>::len)
    }

    /// Whether this event ends the route's data flow in one direction.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::RouteTeardown { .. } | Self::OutputClosed { .. })
    }

    /// Compares two events with `f64` bit patterns rather than IEEE equality.
    ///
    /// [`PartialEq`] follows IEEE-754, under which `NaN != NaN`, so a
    /// round-trip test on a payload whose metadata holds a `NaN` parameter
    /// would fail even though every bit survived. This comparison answers the
    /// question the wire actually cares about: are these the same bytes?
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{Metadata, PeerEvent, RouteId};
    ///
    /// let mut metadata = Metadata::default();
    /// metadata.insert("nan", f64::NAN)?;
    /// let event = PeerEvent::Output {
    ///     route_id: RouteId::FIRST,
    ///     seq: 0,
    ///     metadata,
    ///     payload: Vec::new(),
    /// };
    /// assert!(event != event.clone());
    /// assert!(event.bitwise_eq(&event.clone()));
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    #[must_use]
    pub fn bitwise_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Output {
                    route_id: left_route,
                    seq: left_seq,
                    metadata: left_meta,
                    payload: left_payload,
                },
                Self::Output {
                    route_id: right_route,
                    seq: right_seq,
                    metadata: right_meta,
                    payload: right_payload,
                },
            ) => {
                left_route == right_route
                    && left_seq == right_seq
                    && left_meta.bitwise_eq(right_meta)
                    && left_payload == right_payload
            }
            _ => self == other,
        }
    }
}

impl fmt::Display for PeerEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RouteSetup {
                route_id, route, ..
            } => write!(f, "route setup {route_id} for {}", route.key),
            Self::RouteAccept {
                route_id,
                acceptance,
            } => write!(f, "route {route_id} {acceptance}"),
            Self::RouteTeardown { route_id, reason } => {
                write!(f, "route {route_id} torn down: {reason}")
            }
            Self::Output {
                route_id,
                seq,
                payload,
                ..
            } => write!(
                f,
                "route {route_id} output #{seq} ({} bytes)",
                payload.len()
            ),
            Self::OutputClosed {
                route_id,
                final_seq,
                reason,
            } => write!(
                f,
                "route {route_id} output closed after #{final_seq}: {reason}"
            ),
            Self::Ping {
                nonce, is_reply, ..
            } => {
                if *is_reply {
                    write!(f, "pong {nonce}")
                } else {
                    write!(f, "ping {nonce}")
                }
            }
        }
    }
}

impl_wire_message!(
    PeerEvent,
    FrameKind::PeerEvent,
    [
        "RouteSetup",
        "RouteAccept",
        "RouteTeardown",
        "Output",
        "OutputClosed",
        "Ping",
    ],
    fn variant_index(&self) -> u16 {
        match self {
            Self::RouteSetup { .. } => 0,
            Self::RouteAccept { .. } => 1,
            Self::RouteTeardown { .. } => 2,
            Self::Output { .. } => 3,
            Self::OutputClosed { .. } => 4,
            Self::Ping { .. } => 5,
        }
    }
);

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::codec::{WireDecode, WireEncode, round_trip};
    use crate::common::route::{RouteKey, RouteRejection};
    use crate::common::{Plane, RouteSpec};
    use crate::frame::{Compression, FrameFlags, FrameLimits};
    use crate::ids::DataflowId;
    use crate::messages::WireMessage;

    fn route_key() -> RouteKey {
        RouteKey::new(
            DataflowId::from_u128(0x11),
            "camera/image".parse().unwrap(),
            "detector/frames".parse().unwrap(),
        )
    }

    fn samples() -> Vec<PeerEvent> {
        vec![
            PeerEvent::RouteSetup {
                route_id: RouteId::FIRST,
                route: RouteSpec::new(route_key()).with_plane(Plane::Quic),
                generation: 3,
                type_urn: Some(TypeUrn::new("std/media/v1/Image").unwrap()),
                max_payload_bytes: 1 << 20,
                pool_hint_bytes: Some(8 << 20),
            },
            PeerEvent::RouteAccept {
                route_id: RouteId::FIRST,
                acceptance: RouteAcceptance::accepted(Plane::Quic, Compression::Lz4, 1 << 20),
            },
            PeerEvent::RouteTeardown {
                route_id: RouteId::FIRST,
                reason: RouteCloseReason::DataflowStopped,
            },
            PeerEvent::Output {
                route_id: RouteId::FIRST,
                seq: 42,
                metadata: Metadata::new(HlcTimestamp::new(9, 1)),
                payload: vec![7; 16],
            },
            PeerEvent::OutputClosed {
                route_id: RouteId::FIRST,
                final_seq: 42,
                reason: RouteCloseReason::ProducerFinished,
            },
            PeerEvent::ping(0xFEED, HlcTimestamp::new(1, 0)),
        ]
    }

    #[test]
    fn the_family_has_exactly_the_frozen_variants() {
        assert_eq!(PeerEvent::VARIANT_NAMES.len(), 6);
        assert_eq!(
            PeerEvent::VARIANT_NAMES,
            &[
                "RouteSetup",
                "RouteAccept",
                "RouteTeardown",
                "Output",
                "OutputClosed",
                "Ping",
            ]
        );
    }

    #[test]
    fn every_variant_reports_and_encodes_its_frozen_index() {
        let samples = samples();
        assert_eq!(samples.len(), PeerEvent::VARIANT_NAMES.len());
        for (index, sample) in samples.iter().enumerate() {
            let index = u16::try_from(index).unwrap();
            assert_eq!(sample.variant_index(), index, "{sample:?}");
            let bytes = sample.encode_to_vec().unwrap();
            // The discriminant is a varint written first; below 251 it is one
            // byte equal to the index itself.
            assert_eq!(u16::from(bytes[0]), index, "{sample:?}");
            assert_eq!(
                sample.variant_name(),
                PeerEvent::VARIANT_NAMES[usize::from(index)]
            );
        }
    }

    #[test]
    fn every_variant_round_trips() {
        for sample in samples() {
            assert_eq!(round_trip(&sample).unwrap(), sample);
        }
    }

    #[test]
    fn every_variant_round_trips_through_a_frame() {
        let limits = FrameLimits::network();
        for sample in samples() {
            let bytes = sample.to_frame(FrameFlags::CRC, &limits).unwrap();
            assert_eq!(PeerEvent::from_bytes(&bytes, &limits).unwrap(), sample);
        }
    }

    #[test]
    fn trailing_bytes_after_a_payload_are_refused() {
        for sample in samples() {
            let mut bytes = sample.encode_to_vec().unwrap();
            bytes.push(0);
            assert!(matches!(
                PeerEvent::decode_exact(&bytes),
                Err(crate::error::WireError::TrailingBytes { .. })
            ));
        }
    }

    #[test]
    fn route_accessors_answer_for_every_variant() {
        for sample in samples() {
            match &sample {
                PeerEvent::Ping { .. } => assert!(sample.route_id().is_none()),
                _ => assert_eq!(sample.route_id(), Some(RouteId::FIRST)),
            }
        }
    }

    #[test]
    fn only_output_carries_payload() {
        for sample in samples() {
            if matches!(sample, PeerEvent::Output { .. }) {
                assert!(sample.is_payload());
                assert_eq!(sample.payload_len(), 16);
            } else {
                assert!(!sample.is_payload());
                assert_eq!(sample.payload_len(), 0);
                assert!(sample.payload().is_none());
            }
        }
    }

    #[test]
    fn terminal_events_are_the_two_closing_ones() {
        let terminal: Vec<&'static str> = samples()
            .iter()
            .filter(|event| event.is_terminal())
            .map(PeerEvent::variant_name)
            .collect();
        assert_eq!(terminal, vec!["RouteTeardown", "OutputClosed"]);
    }

    #[test]
    fn a_probe_echoes_once_and_only_once() {
        let probe = PeerEvent::ping(9, HlcTimestamp::new(4, 2));
        let echo = probe.pong().unwrap();
        match echo {
            PeerEvent::Ping {
                nonce,
                sent_at,
                is_reply,
            } => {
                assert_eq!(nonce, 9);
                assert_eq!(sent_at, HlcTimestamp::new(4, 2));
                assert!(is_reply);
            }
            other => panic!("expected a Ping, got {other:?}"),
        }
        assert!(probe.pong().unwrap().pong().is_none());
        assert!(
            PeerEvent::RouteTeardown {
                route_id: RouteId::FIRST,
                reason: RouteCloseReason::ConsumerGone,
            }
            .pong()
            .is_none()
        );
    }

    #[test]
    fn rejections_round_trip_with_their_causes() {
        let rejections = [
            RouteRejection::UnknownDataflow,
            RouteRejection::UnknownPort {
                port: "detector/frames".parse().unwrap(),
            },
            RouteRejection::TypeMismatch {
                expected: Some(TypeUrn::new("std/media/v1/Image").unwrap()),
                found: None,
            },
            RouteRejection::PlaneUnavailable { plane: Plane::Shm },
            RouteRejection::PayloadTooLarge { limit: 1024 },
            RouteRejection::Unauthorized,
            RouteRejection::ShuttingDown,
            RouteRejection::Duplicate {
                route_id: RouteId::new(2),
            },
            RouteRejection::Other {
                message: "no".to_owned(),
            },
        ];
        for (index, reason) in rejections.into_iter().enumerate() {
            let event = PeerEvent::RouteAccept {
                route_id: RouteId::FIRST,
                acceptance: RouteAcceptance::Rejected {
                    reason: reason.clone(),
                },
            };
            assert_eq!(round_trip(&event).unwrap(), event);
            let bytes = reason.encode_to_vec().unwrap();
            assert_eq!(usize::from(bytes[0]), index, "{reason:?}");
        }
    }

    #[test]
    fn close_reasons_round_trip_with_their_causes() {
        let reasons = [
            RouteCloseReason::ProducerFinished,
            RouteCloseReason::ProducerCrashed { generation: 4 },
            RouteCloseReason::ConsumerGone,
            RouteCloseReason::DataflowStopped,
            RouteCloseReason::PlaneFailed {
                plane: Plane::Tcp,
                message: "reset".to_owned(),
            },
            RouteCloseReason::Superseded {
                route_id: RouteId::new(5),
            },
            RouteCloseReason::DaemonShutdown,
            RouteCloseReason::Error {
                message: "boom".to_owned(),
            },
        ];
        for (index, reason) in reasons.into_iter().enumerate() {
            let bytes = reason.encode_to_vec().unwrap();
            assert_eq!(usize::from(bytes[0]), index, "{reason:?}");
            assert_eq!(round_trip(&reason).unwrap(), reason);
        }
    }

    #[test]
    fn nan_metadata_survives_the_wire_even_though_it_is_not_partial_eq() {
        let mut metadata = Metadata::new(HlcTimestamp::new(1, 0));
        metadata.insert("nan", f64::NAN).unwrap();
        let event = PeerEvent::Output {
            route_id: RouteId::FIRST,
            seq: 0,
            metadata,
            payload: vec![1],
        };
        let decoded = round_trip(&event).unwrap();
        assert_ne!(decoded, event);
        assert!(decoded.bitwise_eq(&event));
    }

    #[test]
    fn display_names_every_variant_without_panicking() {
        for sample in samples() {
            let text = sample.to_string();
            assert!(!text.is_empty(), "{sample:?} rendered empty");
        }
    }

    #[test]
    fn a_large_output_stays_within_the_frame_limit() {
        let limits = FrameLimits::network();
        let event = PeerEvent::Output {
            route_id: RouteId::FIRST,
            seq: 1,
            metadata: Metadata::new(HlcTimestamp::new(1, 0)),
            payload: vec![0x5A; 1 << 16],
        };
        let bytes = event.to_frame(FrameFlags::CRC, &limits).unwrap();
        assert_eq!(PeerEvent::from_bytes(&bytes, &limits).unwrap(), event);
    }

    #[test]
    fn an_output_over_the_limit_is_refused_before_encoding() {
        let tight = FrameLimits::uds().with_max_payload_bytes(64);
        let event = PeerEvent::Output {
            route_id: RouteId::FIRST,
            seq: 1,
            metadata: Metadata::new(HlcTimestamp::new(1, 0)),
            payload: vec![0; 1024],
        };
        assert!(matches!(
            event.to_frame(FrameFlags::EMPTY, &tight),
            Err(crate::error::WireError::FrameTooLarge { .. })
        ));
    }

    #[test]
    fn serde_round_trips_the_family_for_diagnostics() {
        for sample in samples() {
            let json = serde_json::to_string(&sample).unwrap();
            let back: PeerEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(back, sample);
        }
    }
}
