//! The seven message families, one module per direction (blueprint §7.3).
//!
//! Every leg of the control plane speaks exactly one request type and one event
//! type, and each of those maps onto exactly one [`FrameKind`], so a router can
//! dispatch on two header bytes without decoding anything:
//!
//! | Leg | Node → peer | Peer → node | Modules |
//! |---|---|---|---|
//! | CLI ↔ coordinator | [`ControlRequest`] | [`ControlReply`] | [`control`] |
//! | coordinator ↔ daemon | [`CoordinatorEvent`] | [`DaemonEvent`] | [`coordinator_daemon`] |
//! | daemon ↔ node | [`NodeRequest`] | [`NodeEvent`] | [`daemon_node`] |
//! | daemon ↔ daemon | [`PeerEvent`] | [`PeerEvent`] | [`peer`] |
//!
//! Fan-out subscriptions (`astrs logs -f`, `astrs topic echo`) ride the same
//! framing under [`FrameKind::Data`], [`FrameKind::Log`] and
//! [`FrameKind::Telemetry`] with a [`crate::SubscriptionId`] in the payload —
//! [`crate::DataFrame`], [`crate::LogFrame`] and [`crate::TelemetryFrame`] are
//! [`WireMessage`]s too, so the same helpers frame them.
//!
//! # The compatibility contract
//!
//! Blueprint §3, principle 4 — *append-only protocol evolution*:
//!
//! - every family enum is `#[non_exhaustive]`, so a downstream `match` cannot
//!   break when a variant is appended;
//! - every variant pins its wire index with `#[oxicode(variant = N)]`, so
//!   reordering the source has no effect on the wire;
//! - the index of an existing variant is **never** changed, and new variants go
//!   at the tail;
//! - `tests/golden/protocol.snap` freezes one encoded sample per variant, so
//!   any accidental renumbering fails CI rather than a robot.
//!
//! [`WireMessage::VARIANT_NAMES`] and [`WireMessage::variant_index`] expose that
//! numbering to routers, metrics labels and the snapshot test itself.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{
//!     AnyMessage, ControlRequest, FrameFlags, FrameKind, FrameLimits, WireMessage, decode_frame,
//! };
//!
//! let limits = FrameLimits::uds();
//! let request = ControlRequest::List { all: true };
//! let bytes = request.to_frame(FrameFlags::EMPTY, &limits)?;
//!
//! // A router only needs the header.
//! let view = decode_frame(&bytes, &limits)?;
//! assert_eq!(view.kind(), FrameKind::Control);
//!
//! // The endpoint decodes the payload.
//! assert_eq!(ControlRequest::from_frame(&view)?, request);
//! assert!(matches!(AnyMessage::from_frame(&view)?, AnyMessage::Control(_)));
//! # Ok::<(), astrs_wire::WireError>(())
//! ```

pub mod any;
pub mod control;
pub mod coordinator_daemon;
pub mod daemon_node;
pub mod peer;
pub mod samples;

pub use any::AnyMessage;
pub use control::{
    ControlReply, ControlRequest, DEFAULT_LOG_LIMIT, DataflowSource, ErrorCode, LogQuery,
    ParamScope, RequestScope, TopicQuery,
};
pub use coordinator_daemon::{
    BuildOutcome, BuildStep, CoordinatorEvent, DaemonEvent, DaemonRegistration, PeerRouteDirective,
    SpawnOutcome, StateEntry, StateEntryKind,
};
pub use daemon_node::{
    DEFAULT_EVENT_BATCH, DEFAULT_ZERO_COPY_THRESHOLD, ENV_NODE_CONFIG, ENV_RUN_PARENT_PID,
    ExtensionKey, ExtensionNamespace, MAX_EXTENSION_NAME_LEN, NodeConfig, NodeConfigError,
    NodeEvent, NodeHandshake, NodeRequest, OutputPayload,
};
pub use peer::PeerEvent;

use crate::codec::{WireDecode, WireEncode};
use crate::error::WireResult;
use crate::frame::{Frame, FrameFlags, FrameKind, FrameLimits, FrameView, write_message};

/// A top-level protocol message: one family enum, one [`FrameKind`].
///
/// The trait is deliberately thin. Encoding and decoding already come from the
/// blanket [`WireEncode`] / [`WireDecode`] implementations over `oxicode`; what
/// a family adds is the *binding* between a Rust type and the header byte that
/// names it, plus the variant numbering the compatibility contract rests on.
///
/// Implementations are provided for the seven §24.1 families and for the three
/// fan-out payload types ([`crate::DataFrame`], [`crate::LogFrame`],
/// [`crate::TelemetryFrame`]).
///
/// # Examples
///
/// ```
/// use astrs_wire::{FrameFlags, FrameKind, FrameLimits, PeerEvent, WireMessage};
///
/// let ping = PeerEvent::Ping {
///     nonce: 7,
///     sent_at: Default::default(),
///     is_reply: false,
/// };
/// assert_eq!(PeerEvent::KIND, FrameKind::PeerEvent);
/// assert_eq!(ping.variant_name(), "Ping");
///
/// let bytes = ping.to_frame(FrameFlags::CRC, &FrameLimits::network())?;
/// assert_eq!(PeerEvent::from_bytes(&bytes, &FrameLimits::network())?, ping);
/// # Ok::<(), astrs_wire::WireError>(())
/// ```
pub trait WireMessage: WireEncode + WireDecode + Sized {
    /// The frame family this message travels in.
    const KIND: FrameKind;

    /// Every variant name, in wire-index order.
    ///
    /// The index of a name in this slice **is** its wire discriminant, which is
    /// what makes the protocol snapshot a numbering check and not merely a byte
    /// check.
    const VARIANT_NAMES: &'static [&'static str];

    /// This value's wire discriminant.
    ///
    /// For a struct-shaped message (the fan-out frames) this is always `0`.
    fn variant_index(&self) -> u16;

    /// This value's variant name, as it appears in the protocol snapshot.
    ///
    /// Falls back to `"<unknown>"` for a discriminant with no name, which
    /// cannot happen for a value this build constructed and is therefore only
    /// reachable if [`WireMessage::VARIANT_NAMES`] is left short of the enum —
    /// a mistake the family tests catch.
    fn variant_name(&self) -> &'static str {
        Self::VARIANT_NAMES
            .get(usize::from(self.variant_index()))
            .copied()
            .unwrap_or("<unknown>")
    }

    /// Appends this message to `out` as a complete frame.
    ///
    /// Returns the number of bytes appended; `out` is restored to its original
    /// length if anything fails.
    ///
    /// # Errors
    ///
    /// As [`write_message`].
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{FrameFlags, FrameLimits, PeerEvent, WireMessage};
    ///
    /// let mut out = Vec::new();
    /// let event = PeerEvent::Ping {
    ///     nonce: 1,
    ///     sent_at: Default::default(),
    ///     is_reply: true,
    /// };
    /// let written = event.write_frame_into(&mut out, FrameFlags::EMPTY, &FrameLimits::uds())?;
    /// assert_eq!(written, out.len());
    /// # Ok::<(), astrs_wire::WireError>(())
    /// ```
    fn write_frame_into(
        &self,
        out: &mut Vec<u8>,
        flags: FrameFlags,
        limits: &FrameLimits,
    ) -> WireResult<usize> {
        write_message(out, Self::KIND, flags, self, limits)
    }

    /// Encodes this message as a complete frame in a fresh buffer.
    ///
    /// # Errors
    ///
    /// As [`write_message`].
    fn to_frame(&self, flags: FrameFlags, limits: &FrameLimits) -> WireResult<Vec<u8>> {
        let mut out = Vec::new();
        self.write_frame_into(&mut out, flags, limits)?;
        Ok(out)
    }

    /// Builds an owned [`Frame`] carrying this message.
    ///
    /// # Errors
    ///
    /// As [`Frame::from_message`].
    fn to_owned_frame(&self, flags: FrameFlags, limits: &FrameLimits) -> WireResult<Frame> {
        Frame::from_message(Self::KIND, flags, self, limits)
    }

    /// Decodes this message from a frame, checking the family first.
    ///
    /// # Errors
    ///
    /// - [`crate::WireError::KindMismatch`] if the frame belongs to another
    ///   family.
    /// - [`crate::WireError::CompressedPayload`] if the payload is still
    ///   compressed.
    /// - [`crate::WireError::Codec`] or [`crate::WireError::TrailingBytes`]
    ///   from the codec.
    fn from_frame(view: &FrameView<'_>) -> WireResult<Self> {
        view.decode_as(Self::KIND)
    }

    /// Decodes this message from a payload slice, without a frame around it.
    ///
    /// Used by transports that already framed the bytes themselves (a QUIC
    /// datagram, a SHM slot) and by the snapshot tests.
    ///
    /// # Errors
    ///
    /// As [`WireDecode::decode_exact`].
    fn from_payload(payload: &[u8]) -> WireResult<Self> {
        Self::decode_exact(payload)
    }

    /// Decodes this message from a buffer holding exactly one complete frame.
    ///
    /// # Errors
    ///
    /// As [`crate::decode_frame`] and [`WireMessage::from_frame`].
    fn from_bytes(bytes: &[u8], limits: &FrameLimits) -> WireResult<Self> {
        let view = crate::frame::decode_frame(bytes, limits)?;
        Self::from_frame(&view)
    }
}

/// Implements [`WireMessage`] for a family, with its kind and variant table.
///
/// The `variant_index` body is written out per family rather than generated,
/// because an exhaustive `match` is what makes the compiler refuse a variant
/// that was appended without being numbered.
macro_rules! impl_wire_message {
    ($ty:ty, $kind:expr, [$($name:literal),* $(,)?], $index:item) => {
        impl $crate::messages::WireMessage for $ty {
            const KIND: $crate::frame::FrameKind = $kind;
            const VARIANT_NAMES: &'static [&'static str] = &[$($name),*];
            $index
        }
    };
}

pub(crate) use impl_wire_message;

impl WireMessage for crate::common::DataFrame {
    const KIND: FrameKind = FrameKind::Data;
    const VARIANT_NAMES: &'static [&'static str] = &["DataFrame"];

    fn variant_index(&self) -> u16 {
        0
    }
}

impl WireMessage for crate::common::LogFrame {
    const KIND: FrameKind = FrameKind::Log;
    const VARIANT_NAMES: &'static [&'static str] = &["LogFrame"];

    fn variant_index(&self) -> u16 {
        0
    }
}

impl WireMessage for crate::common::TelemetryFrame {
    const KIND: FrameKind = FrameKind::Telemetry;
    const VARIANT_NAMES: &'static [&'static str] = &["TelemetryFrame"];

    fn variant_index(&self) -> u16 {
        0
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_time::HlcTimestamp;

    use super::*;
    use crate::common::{DataFrame, LogFrame, LogLevel, LogRecord};
    use crate::error::WireError;
    use crate::ids::{DataflowId, SubscriptionId};
    use crate::metadata::Metadata;

    fn data_frame() -> DataFrame {
        DataFrame::new(
            SubscriptionId::new(3),
            DataflowId::from_u128(7),
            "camera/image".parse().unwrap(),
            Metadata::new(HlcTimestamp::new(1_000, 0)),
            vec![1, 2, 3],
        )
    }

    #[test]
    fn fan_out_frames_carry_their_own_kinds() {
        assert_eq!(DataFrame::KIND, FrameKind::Data);
        assert_eq!(LogFrame::KIND, FrameKind::Log);
        assert_eq!(
            <crate::common::TelemetryFrame as WireMessage>::KIND,
            FrameKind::Telemetry
        );
    }

    #[test]
    fn a_data_frame_round_trips_through_its_own_helpers() {
        let limits = FrameLimits::uds();
        let frame = data_frame();
        let bytes = frame.to_frame(FrameFlags::EMPTY, &limits).unwrap();
        let decoded = DataFrame::from_bytes(&bytes, &limits).unwrap();
        assert!(decoded.bitwise_eq(&frame));
        assert_eq!(frame.variant_name(), "DataFrame");
    }

    #[test]
    fn a_family_mismatch_is_reported_not_guessed() {
        let limits = FrameLimits::uds();
        let bytes = data_frame().to_frame(FrameFlags::EMPTY, &limits).unwrap();
        let view = crate::frame::decode_frame(&bytes, &limits).unwrap();
        match LogFrame::from_frame(&view) {
            Err(WireError::KindMismatch { expected, found }) => {
                assert_eq!(expected, FrameKind::Log);
                assert_eq!(found, FrameKind::Data);
            }
            other => panic!("expected KindMismatch, got {other:?}"),
        }
    }

    #[test]
    fn log_frames_frame_and_unframe() {
        let limits = FrameLimits::network();
        let record = LogRecord::new(HlcTimestamp::new(5, 1), LogLevel::Warn, "queue full");
        let frame = LogFrame::new(SubscriptionId::FIRST, record);
        let bytes = frame.to_frame(FrameFlags::CRC, &limits).unwrap();
        assert_eq!(LogFrame::from_bytes(&bytes, &limits).unwrap(), frame);
    }

    #[test]
    fn owned_frames_and_byte_frames_agree() {
        let limits = FrameLimits::uds();
        let frame = data_frame();
        let owned = frame.to_owned_frame(FrameFlags::EMPTY, &limits).unwrap();
        let bytes = frame.to_frame(FrameFlags::EMPTY, &limits).unwrap();
        assert_eq!(owned.encode(&limits).unwrap(), bytes);
    }
}
