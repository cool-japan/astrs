//! The `ASTRS-MUX/1` frame header and body encodings.
//!
//! This module is the normative spelling of the mini-protocol described in
//! [`crate::mux`]. Everything here is byte-exact and hand-encoded: the header
//! is on the hot path of every frame on a muxed connection, so it is nine
//! fixed bytes read with `from_le_bytes`, not an `oxicode` decode.
//!
//! # Layout
//!
//! ```text
//!  byte 0    bytes 1..9                        bytes 9..
//! ┌────────┬──────────────────────────────────┬──────────────────────────┐
//! │ tag u8 │ route: u64 little-endian         │ body                     │
//! └────────┴──────────────────────────────────┴──────────────────────────┘
//! ```
//!
//! The header sits at the front of the *frame payload* — inside
//! `astrs-wire`'s `magic | ver | flags | kind | len | payload | crc32c`, never
//! beside it. The frame codec is used exactly as `astrs-wire` defines it; what
//! this crate adds is a convention about the first nine payload bytes, in the
//! same way the compression container (see [`crate::compress`]) is a
//! convention about the rest of them.
//!
//! # Bodies
//!
//! | Tag | Body |
//! |---|---|
//! | [`MuxTag::Control`] | the real frame payload |
//! | [`MuxTag::RouteData`] | the real frame payload, possibly compressed |
//! | [`MuxTag::RouteOpen`] | `credit: u32 LE` ‖ opaque descriptor |
//! | [`MuxTag::RouteAccept`] | `credit: u32 LE` ‖ `status: u8` ‖ UTF-8 detail |
//! | [`MuxTag::RouteClose`] | `code: u16 LE` ‖ UTF-8 detail |
//! | [`MuxTag::Credit`] | `delta: u32 LE` |
//! | [`MuxTag::Datagram`] | the real frame payload, best-effort |
//!
//! # Examples
//!
//! ```
//! use astrs_transport::mux::{MuxHeader, MuxTag};
//! use astrs_wire::RouteId;
//!
//! let mut wire = Vec::new();
//! MuxHeader::new(MuxTag::RouteData, RouteId::new(9)).encode_into(&mut wire);
//! wire.extend_from_slice(b"an arrow ipc stream");
//!
//! let (header, body) = MuxHeader::decode(&wire)?;
//! assert_eq!(header.tag, MuxTag::RouteData);
//! assert_eq!(header.route, RouteId::new(9));
//! assert_eq!(body, b"an arrow ipc stream");
//! # Ok::<(), astrs_transport::MuxHeaderError>(())
//! ```

use astrs_wire::RouteId;

use crate::error::MuxHeaderError;

/// The fixed size of a mux header, in bytes.
pub const MUX_HEADER_LEN: usize = 9;

/// The offset of the route handle within the header.
const ROUTE_OFFSET: usize = 1;

/// The mini-protocol version this build speaks.
///
/// Not carried on the wire: both ends of a connection run the same
/// `astrs-transport` release, and the frame-level `ver` byte plus the
/// handshake's `protocol` already pin the pair (§7.2). The constant exists so
/// that the module's documentation and its tests name the same thing.
pub const MUX_PROTOCOL: &str = "ASTRS-MUX/1";

/// The status byte of an accepted route.
pub const ACCEPT_STATUS_OK: u8 = 0;
/// The status byte of a refused route.
pub const ACCEPT_STATUS_REFUSED: u8 = 1;

/// Close code: the route finished normally.
pub const CLOSE_CODE_NORMAL: u16 = 0;
/// Close code: the peer refused to open the route.
pub const CLOSE_CODE_REFUSED: u16 = 1;
/// Close code: the peer exceeded its flow-control window.
pub const CLOSE_CODE_FLOW_CONTROL: u16 = 2;
/// Close code: the connection is at its route ceiling.
pub const CLOSE_CODE_ROUTE_LIMIT: u16 = 3;
/// Close code: the route handle was already in use.
pub const CLOSE_CODE_DUPLICATE: u16 = 4;
/// Close code: the endpoint is shutting down.
pub const CLOSE_CODE_SHUTDOWN: u16 = 5;

/// What a muxed frame is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
#[non_exhaustive]
pub enum MuxTag {
    /// A control-plane frame: "stream 0" (§6.4). Route must be
    /// [`RouteId::NONE`].
    Control = 0,
    /// A payload on an open route.
    RouteData = 1,
    /// The sender is opening a logical stream.
    RouteOpen = 2,
    /// The answer to a [`MuxTag::RouteOpen`].
    RouteAccept = 3,
    /// The sender is closing a logical stream.
    RouteClose = 4,
    /// The sender is granting the peer more flow-control window.
    Credit = 5,
    /// A best-effort payload with no delivery guarantee. Route must be
    /// [`RouteId::NONE`].
    Datagram = 6,
}

impl MuxTag {
    /// Every tag this build knows, in wire order.
    pub const ALL: &'static [Self] = &[
        Self::Control,
        Self::RouteData,
        Self::RouteOpen,
        Self::RouteAccept,
        Self::RouteClose,
        Self::Credit,
        Self::Datagram,
    ];

    /// The wire byte.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Parses a wire byte.
    ///
    /// # Errors
    ///
    /// [`MuxHeaderError::UnknownTag`] for a byte this build has no name for.
    /// Unknown tags are refused rather than ignored: a frame whose meaning is
    /// unknown may not simply be dropped, because the flow-control accounting
    /// on the peer's side already counted it.
    pub const fn from_u8(value: u8) -> Result<Self, MuxHeaderError> {
        match value {
            0 => Ok(Self::Control),
            1 => Ok(Self::RouteData),
            2 => Ok(Self::RouteOpen),
            3 => Ok(Self::RouteAccept),
            4 => Ok(Self::RouteClose),
            5 => Ok(Self::Credit),
            6 => Ok(Self::Datagram),
            tag => Err(MuxHeaderError::UnknownTag { tag }),
        }
    }

    /// A stable name for logs and errors.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Control => "control",
            Self::RouteData => "route_data",
            Self::RouteOpen => "route_open",
            Self::RouteAccept => "route_accept",
            Self::RouteClose => "route_close",
            Self::Credit => "credit",
            Self::Datagram => "datagram",
        }
    }

    /// Whether this tag addresses a specific route.
    ///
    /// [`MuxTag::Control`] and [`MuxTag::Datagram`] do not; everything else
    /// does, and carrying [`RouteId::NONE`] with one of them is a protocol
    /// violation.
    #[must_use]
    pub const fn is_route_scoped(self) -> bool {
        !matches!(self, Self::Control | Self::Datagram)
    }

    /// Whether this tag's body counts against the route's flow-control window.
    ///
    /// Only payload frames do. Making a credit grant itself consume credit
    /// would deadlock the connection the first time a window emptied.
    #[must_use]
    pub const fn consumes_credit(self) -> bool {
        matches!(self, Self::RouteData)
    }

    /// Whether this tag's body may be compressed.
    #[must_use]
    pub const fn may_compress(self) -> bool {
        matches!(self, Self::RouteData | Self::Control | Self::Datagram)
    }
}

impl std::fmt::Display for MuxTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The nine bytes at the front of every muxed frame payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MuxHeader {
    /// What this frame is.
    pub tag: MuxTag,
    /// Which logical stream it belongs to.
    pub route: RouteId,
}

impl MuxHeader {
    /// A header for `tag` on `route`.
    #[must_use]
    pub const fn new(tag: MuxTag, route: RouteId) -> Self {
        Self { tag, route }
    }

    /// A control-plane header.
    #[must_use]
    pub const fn control() -> Self {
        Self::new(MuxTag::Control, RouteId::NONE)
    }

    /// A datagram header.
    #[must_use]
    pub const fn datagram() -> Self {
        Self::new(MuxTag::Datagram, RouteId::NONE)
    }

    /// Appends this header to `out`.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        out.reserve(MUX_HEADER_LEN);
        out.push(self.tag.as_u8());
        out.extend_from_slice(&self.route.get().to_le_bytes());
    }

    /// The header as a fixed-size array, for a caller building a frame in
    /// place.
    #[must_use]
    pub const fn to_bytes(&self) -> [u8; MUX_HEADER_LEN] {
        let route = self.route.get().to_le_bytes();
        [
            self.tag.as_u8(),
            route[0],
            route[1],
            route[2],
            route[3],
            route[4],
            route[5],
            route[6],
            route[7],
        ]
    }

    /// Splits a frame payload into its header and body.
    ///
    /// # Errors
    ///
    /// - [`MuxHeaderError::Truncated`] if the payload is shorter than the
    ///   header.
    /// - [`MuxHeaderError::UnknownTag`] for an unrecognised tag byte.
    /// - [`MuxHeaderError::RouteMismatch`] if a route-scoped tag carries
    ///   [`RouteId::NONE`], or a connection-scoped tag carries a route.
    pub fn decode(payload: &[u8]) -> Result<(Self, &[u8]), MuxHeaderError> {
        if payload.len() < MUX_HEADER_LEN {
            return Err(MuxHeaderError::Truncated {
                len: payload.len(),
                needed: MUX_HEADER_LEN,
            });
        }
        let tag = MuxTag::from_u8(payload[0])?;
        let mut route_bytes = [0u8; 8];
        route_bytes.copy_from_slice(&payload[ROUTE_OFFSET..MUX_HEADER_LEN]);
        let route = RouteId::new(u64::from_le_bytes(route_bytes));

        if tag.is_route_scoped() == route.is_none() {
            return Err(MuxHeaderError::RouteMismatch {
                tag: tag.as_str(),
                route,
            });
        }
        Ok((Self { tag, route }, &payload[MUX_HEADER_LEN..]))
    }
}

/// The body of a [`MuxTag::RouteOpen`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenBody {
    /// How many frames the opener will accept before it must grant more.
    pub credit: u32,
    /// An opaque descriptor the application attached to the open.
    ///
    /// The transport never interprets it. A daemon puts an encoded
    /// [`astrs_wire::RouteSpec`] here so that the peer's *application* layer
    /// can decide the route's terms in the same round trip the transport uses
    /// to create the demux slot.
    pub descriptor: Vec<u8>,
}

impl OpenBody {
    /// A body granting `credit` with no descriptor.
    #[must_use]
    pub const fn new(credit: u32) -> Self {
        Self {
            credit,
            descriptor: Vec::new(),
        }
    }

    /// Attaches an application descriptor.
    #[must_use]
    pub fn with_descriptor(mut self, descriptor: impl Into<Vec<u8>>) -> Self {
        self.descriptor = descriptor.into();
        self
    }

    /// Appends this body to `out`.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.credit.to_le_bytes());
        out.extend_from_slice(&self.descriptor);
    }

    /// Parses a body.
    ///
    /// # Errors
    ///
    /// [`MuxHeaderError::BadBody`] if the credit field is missing.
    pub fn decode(body: &[u8]) -> Result<Self, MuxHeaderError> {
        let credit = read_u32(body, MuxTag::RouteOpen)?;
        Ok(Self {
            credit,
            descriptor: body[4..].to_vec(),
        })
    }
}

/// The body of a [`MuxTag::RouteAccept`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptBody {
    /// How many frames the accepting end will take before granting more.
    pub credit: u32,
    /// Whether the route was accepted.
    pub accepted: bool,
    /// A human-readable detail, empty when there is nothing to say.
    pub detail: String,
}

impl AcceptBody {
    /// An acceptance granting `credit`.
    #[must_use]
    pub const fn accepted(credit: u32) -> Self {
        Self {
            credit,
            accepted: true,
            detail: String::new(),
        }
    }

    /// A refusal with a stated cause.
    #[must_use]
    pub fn refused(detail: impl Into<String>) -> Self {
        Self {
            credit: 0,
            accepted: false,
            detail: detail.into(),
        }
    }

    /// Appends this body to `out`.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.credit.to_le_bytes());
        out.push(if self.accepted {
            ACCEPT_STATUS_OK
        } else {
            ACCEPT_STATUS_REFUSED
        });
        out.extend_from_slice(self.detail.as_bytes());
    }

    /// Parses a body.
    ///
    /// # Errors
    ///
    /// [`MuxHeaderError::BadBody`] if the fixed fields are missing.
    pub fn decode(body: &[u8]) -> Result<Self, MuxHeaderError> {
        let credit = read_u32(body, MuxTag::RouteAccept)?;
        let status = *body.get(4).ok_or(MuxHeaderError::BadBody {
            tag: MuxTag::RouteAccept.as_str(),
            expected: 5,
            found: body.len(),
        })?;
        Ok(Self {
            credit,
            accepted: status == ACCEPT_STATUS_OK,
            detail: String::from_utf8_lossy(&body[5..]).into_owned(),
        })
    }
}

/// The body of a [`MuxTag::RouteClose`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseBody {
    /// A machine-readable cause, one of the `CLOSE_CODE_*` constants.
    pub code: u16,
    /// A human-readable detail, empty when there is nothing to say.
    pub detail: String,
}

impl CloseBody {
    /// A close with `code` and no detail.
    #[must_use]
    pub const fn new(code: u16) -> Self {
        Self {
            code,
            detail: String::new(),
        }
    }

    /// A close with `code` and a stated cause.
    #[must_use]
    pub fn with_detail(code: u16, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    /// Appends this body to `out`.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.code.to_le_bytes());
        out.extend_from_slice(self.detail.as_bytes());
    }

    /// Parses a body.
    ///
    /// # Errors
    ///
    /// [`MuxHeaderError::BadBody`] if the code is missing.
    pub fn decode(body: &[u8]) -> Result<Self, MuxHeaderError> {
        let code_bytes: [u8; 2] = body
            .get(..2)
            .and_then(|slice| slice.try_into().ok())
            .ok_or(MuxHeaderError::BadBody {
                tag: MuxTag::RouteClose.as_str(),
                expected: 2,
                found: body.len(),
            })?;
        Ok(Self {
            code: u16::from_le_bytes(code_bytes),
            detail: String::from_utf8_lossy(&body[2..]).into_owned(),
        })
    }

    /// A short, stable name for this close code.
    #[must_use]
    pub const fn code_name(&self) -> &'static str {
        match self.code {
            CLOSE_CODE_NORMAL => "normal",
            CLOSE_CODE_REFUSED => "refused",
            CLOSE_CODE_FLOW_CONTROL => "flow_control",
            CLOSE_CODE_ROUTE_LIMIT => "route_limit",
            CLOSE_CODE_DUPLICATE => "duplicate",
            CLOSE_CODE_SHUTDOWN => "shutdown",
            _ => "unknown",
        }
    }
}

/// Encodes a [`MuxTag::Credit`] body.
#[must_use]
pub fn encode_credit(delta: u32) -> [u8; 4] {
    delta.to_le_bytes()
}

/// Parses a [`MuxTag::Credit`] body.
///
/// # Errors
///
/// [`MuxHeaderError::BadBody`] if the body is not exactly four bytes.
pub fn decode_credit(body: &[u8]) -> Result<u32, MuxHeaderError> {
    if body.len() != 4 {
        return Err(MuxHeaderError::BadBody {
            tag: MuxTag::Credit.as_str(),
            expected: 4,
            found: body.len(),
        });
    }
    read_u32(body, MuxTag::Credit)
}

/// Reads a little-endian `u32` from the front of `body`.
fn read_u32(body: &[u8], tag: MuxTag) -> Result<u32, MuxHeaderError> {
    let bytes: [u8; 4] = body
        .get(..4)
        .and_then(|slice| slice.try_into().ok())
        .ok_or(MuxHeaderError::BadBody {
            tag: tag.as_str(),
            expected: 4,
            found: body.len(),
        })?;
    Ok(u32::from_le_bytes(bytes))
}

/// Builds a complete muxed payload: header, then body.
///
/// Used on the send path, where the body is already encoded (and possibly
/// compressed).
///
/// # Examples
///
/// ```
/// use astrs_transport::mux::{MuxHeader, MuxTag, build_payload};
/// use astrs_wire::RouteId;
///
/// let payload = build_payload(MuxHeader::new(MuxTag::RouteData, RouteId::FIRST), b"body");
/// let (header, body) = MuxHeader::decode(&payload)?;
/// assert_eq!(header.tag, MuxTag::RouteData);
/// assert_eq!(body, b"body");
/// # Ok::<(), astrs_transport::MuxHeaderError>(())
/// ```
#[must_use]
pub fn build_payload(header: MuxHeader, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(MUX_HEADER_LEN + body.len());
    header.encode_into(&mut out);
    out.extend_from_slice(body);
    out
}

/// Builds a complete muxed payload into a caller-owned buffer.
///
/// The buffer is cleared first. Reusing one buffer per route is what keeps a
/// high-rate route from allocating once per frame.
pub fn build_payload_into(header: MuxHeader, body: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(MUX_HEADER_LEN + body.len());
    header.encode_into(out);
    out.extend_from_slice(body);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn every_tag_round_trips_through_its_byte() {
        for (index, &tag) in MuxTag::ALL.iter().enumerate() {
            assert_eq!(tag.as_u8() as usize, index, "{tag} moved");
            assert_eq!(MuxTag::from_u8(tag.as_u8()).unwrap(), tag);
            assert!(!tag.as_str().is_empty());
            assert_eq!(tag.to_string(), tag.as_str());
        }
    }

    #[test]
    fn an_unknown_tag_is_refused() {
        for byte in 7u8..=255 {
            assert_eq!(
                MuxTag::from_u8(byte),
                Err(MuxHeaderError::UnknownTag { tag: byte })
            );
        }
    }

    #[test]
    fn the_header_is_exactly_nine_bytes() {
        let header = MuxHeader::new(MuxTag::RouteData, RouteId::new(0x0102_0304_0506_0708));
        let mut out = Vec::new();
        header.encode_into(&mut out);
        assert_eq!(out.len(), MUX_HEADER_LEN);
        assert_eq!(out[0], MuxTag::RouteData.as_u8());
        assert_eq!(&out[1..], &0x0102_0304_0506_0708u64.to_le_bytes());
        assert_eq!(header.to_bytes().as_slice(), out.as_slice());
    }

    #[test]
    fn a_header_round_trips_with_its_body() {
        for &tag in MuxTag::ALL {
            let route = if tag.is_route_scoped() {
                RouteId::new(77)
            } else {
                RouteId::NONE
            };
            let payload = build_payload(MuxHeader::new(tag, route), b"the body");
            let (header, body) = MuxHeader::decode(&payload).unwrap();
            assert_eq!(header.tag, tag);
            assert_eq!(header.route, route);
            assert_eq!(body, b"the body");
        }
    }

    #[test]
    fn a_truncated_payload_is_a_typed_error() {
        for len in 0..MUX_HEADER_LEN {
            let err = MuxHeader::decode(&vec![0u8; len]).unwrap_err();
            assert_eq!(
                err,
                MuxHeaderError::Truncated {
                    len,
                    needed: MUX_HEADER_LEN
                }
            );
        }
    }

    #[test]
    fn a_route_scoped_tag_may_not_carry_the_reserved_route() {
        for &tag in MuxTag::ALL {
            if !tag.is_route_scoped() {
                continue;
            }
            let payload = build_payload(MuxHeader::new(tag, RouteId::NONE), b"");
            let err = MuxHeader::decode(&payload).unwrap_err();
            assert!(matches!(err, MuxHeaderError::RouteMismatch { .. }), "{tag}");
        }
    }

    #[test]
    fn a_connection_scoped_tag_may_not_carry_a_route() {
        for tag in [MuxTag::Control, MuxTag::Datagram] {
            let payload = build_payload(MuxHeader::new(tag, RouteId::FIRST), b"");
            let err = MuxHeader::decode(&payload).unwrap_err();
            assert!(matches!(err, MuxHeaderError::RouteMismatch { .. }), "{tag}");
        }
    }

    #[test]
    fn only_payload_frames_consume_credit() {
        assert!(MuxTag::RouteData.consumes_credit());
        for tag in [
            MuxTag::Control,
            MuxTag::RouteOpen,
            MuxTag::RouteAccept,
            MuxTag::RouteClose,
            MuxTag::Credit,
            MuxTag::Datagram,
        ] {
            assert!(!tag.consumes_credit(), "{tag} must not consume credit");
        }
    }

    #[test]
    fn only_payload_carrying_tags_may_compress() {
        assert!(MuxTag::RouteData.may_compress());
        assert!(MuxTag::Control.may_compress());
        assert!(MuxTag::Datagram.may_compress());
        for tag in [
            MuxTag::RouteOpen,
            MuxTag::RouteAccept,
            MuxTag::RouteClose,
            MuxTag::Credit,
        ] {
            assert!(!tag.may_compress(), "{tag} must never be compressed");
        }
    }

    #[test]
    fn an_open_body_round_trips() {
        let body = OpenBody::new(32).with_descriptor(b"route-spec".to_vec());
        let mut out = Vec::new();
        body.encode_into(&mut out);
        assert_eq!(OpenBody::decode(&out).unwrap(), body);

        let bare = OpenBody::new(1);
        let mut out = Vec::new();
        bare.encode_into(&mut out);
        let decoded = OpenBody::decode(&out).unwrap();
        assert_eq!(decoded.credit, 1);
        assert!(decoded.descriptor.is_empty());
    }

    #[test]
    fn a_short_open_body_is_a_typed_error() {
        for len in 0..4 {
            let err = OpenBody::decode(&vec![0u8; len]).unwrap_err();
            assert!(matches!(err, MuxHeaderError::BadBody { .. }));
        }
    }

    #[test]
    fn an_accept_body_round_trips_both_ways() {
        let ok = AcceptBody::accepted(64);
        let mut out = Vec::new();
        ok.encode_into(&mut out);
        assert_eq!(AcceptBody::decode(&out).unwrap(), ok);

        let no = AcceptBody::refused("no such dataflow");
        let mut out = Vec::new();
        no.encode_into(&mut out);
        let decoded = AcceptBody::decode(&out).unwrap();
        assert!(!decoded.accepted);
        assert_eq!(decoded.detail, "no such dataflow");
        assert_eq!(decoded.credit, 0);
    }

    #[test]
    fn a_short_accept_body_is_a_typed_error() {
        for len in 0..5 {
            let err = AcceptBody::decode(&vec![0u8; len]).unwrap_err();
            assert!(matches!(err, MuxHeaderError::BadBody { .. }));
        }
    }

    #[test]
    fn an_unrecognised_accept_status_reads_as_a_refusal() {
        // Fail closed: a status byte from a future release is not "accepted".
        let mut out = 8u32.to_le_bytes().to_vec();
        out.push(200);
        assert!(!AcceptBody::decode(&out).unwrap().accepted);
    }

    #[test]
    fn a_close_body_round_trips_and_names_its_code() {
        for (code, name) in [
            (CLOSE_CODE_NORMAL, "normal"),
            (CLOSE_CODE_REFUSED, "refused"),
            (CLOSE_CODE_FLOW_CONTROL, "flow_control"),
            (CLOSE_CODE_ROUTE_LIMIT, "route_limit"),
            (CLOSE_CODE_DUPLICATE, "duplicate"),
            (CLOSE_CODE_SHUTDOWN, "shutdown"),
            (9_999, "unknown"),
        ] {
            let body = CloseBody::with_detail(code, "because");
            assert_eq!(body.code_name(), name);
            let mut out = Vec::new();
            body.encode_into(&mut out);
            assert_eq!(CloseBody::decode(&out).unwrap(), body);
        }
        assert!(CloseBody::new(CLOSE_CODE_NORMAL).detail.is_empty());
    }

    #[test]
    fn a_short_close_body_is_a_typed_error() {
        for len in 0..2 {
            assert!(CloseBody::decode(&vec![0u8; len]).is_err());
        }
    }

    #[test]
    fn a_credit_body_is_exactly_four_bytes() {
        assert_eq!(decode_credit(&encode_credit(1_234)).unwrap(), 1_234);
        assert!(decode_credit(&[0, 0, 0]).is_err());
        assert!(decode_credit(&[0, 0, 0, 0, 0]).is_err());
    }

    #[test]
    fn a_non_utf8_detail_is_replaced_rather_than_rejected() {
        // A detail string is diagnostic, never load-bearing: bad bytes must
        // not turn a route close into a connection failure.
        let mut out = CLOSE_CODE_NORMAL.to_le_bytes().to_vec();
        out.extend_from_slice(&[0xff, 0xfe, 0xfd]);
        let body = CloseBody::decode(&out).unwrap();
        assert_eq!(body.code, CLOSE_CODE_NORMAL);
        assert!(!body.detail.is_empty());
    }

    #[test]
    fn the_buffered_builder_matches_the_allocating_one() {
        let header = MuxHeader::new(MuxTag::RouteData, RouteId::new(5));
        let mut scratch = vec![0xaa; 100];
        build_payload_into(header, b"payload", &mut scratch);
        assert_eq!(scratch, build_payload(header, b"payload"));
    }

    #[test]
    fn the_convenience_constructors_use_the_reserved_route() {
        assert_eq!(MuxHeader::control().route, RouteId::NONE);
        assert_eq!(MuxHeader::control().tag, MuxTag::Control);
        assert_eq!(MuxHeader::datagram().route, RouteId::NONE);
        assert_eq!(MuxHeader::datagram().tag, MuxTag::Datagram);
    }

    #[test]
    fn the_protocol_name_is_stable() {
        assert_eq!(MUX_PROTOCOL, "ASTRS-MUX/1");
    }
}
