//! The broker's request/reply framing.
//!
//! A tiny, hand-rolled binary protocol rather than a reuse of
//! `astrs-wire`'s frame format, for one reason: this channel carries **file
//! descriptors**, and its message boundaries have to line up with `sendmsg`
//! ancillary data. Borrowing the control-plane codec would mean adding
//! `FrameKind` variants to a crate whose enum indices are frozen at first
//! release (§7.2, §24.1) for a channel no other component speaks.
//!
//! # Frame
//!
//! ```text
//! 0      4     5     6        8              12            12 + len
//! ┌──────┬─────┬─────┬────────┬──────────────┬─────────────────┐
//! │"ASBK"│ ver │ op  │ flags  │ payload_len  │ payload         │
//! │  4 B │ 1 B │ 1 B │  2 B   │    4 B (LE)  │ payload_len B   │
//! └──────┴─────┴─────┴────────┴──────────────┴─────────────────┘
//! ```
//!
//! Little-endian throughout. The channel is a Unix socket between processes
//! on one host, so byte order is a formality — but an explicit one, because
//! `astrs replay` reproduces recorded broker exchanges and those bytes must
//! decode identically on any host.
//!
//! # Examples
//!
//! ```
//! # #[cfg(unix)] {
//! use astrs_shm::protocol::{AttachRequest, BrokerFrame, Opcode};
//!
//! let request = AttachRequest { key_digest: 42, generation: 7 };
//! let frame = BrokerFrame::new(Opcode::Attach, request.encode());
//! let bytes = frame.encode();
//!
//! let (opcode, len) = BrokerFrame::decode_header(&bytes[..12])?;
//! assert_eq!(opcode, Opcode::Attach);
//! assert_eq!(len, bytes.len() - 12);
//! assert_eq!(AttachRequest::decode(&bytes[12..])?, request);
//! # }
//! # Ok::<(), astrs_shm::ShmError>(())
//! ```

use crate::error::{ShmError, ShmResult};

/// The magic at the start of every broker frame.
pub const BROKER_MAGIC: [u8; 4] = *b"ASBK";

/// The broker protocol version this build speaks.
pub const BROKER_VERSION: u8 = 1;

/// The fixed frame header length, in bytes.
pub const BROKER_HEADER_LEN: usize = 12;

/// The largest payload a broker frame may carry.
///
/// Broker payloads are fixed-shape structs of at most a few dozen bytes, plus
/// refusal strings; the cap exists so a hostile peer cannot make the broker
/// allocate on a forged length prefix.
pub const MAX_BROKER_PAYLOAD: usize = 4096;

/// What a broker frame is asking for, or answering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
#[non_exhaustive]
pub enum Opcode {
    /// Client → broker: "give me the descriptor for this segment".
    Attach = 1,
    /// Broker → client: descriptor attached as ancillary data.
    AttachReply = 2,
    /// Broker → client: request refused, payload is a UTF-8 reason.
    Refused = 3,
    /// Client → broker: "wire this doorbell to the producer", descriptor
    /// attached.
    RegisterDoorbell = 4,
    /// Broker → client: request accepted, nothing to return.
    Ack = 5,
    /// Client → broker: liveness probe.
    Ping = 6,
    /// Broker → client: liveness answer.
    Pong = 7,
    /// Client → broker: mark the segment closed (a producer shutting down
    /// cleanly, or a supervisor retiring a route).
    CloseSegment = 8,
    /// Client → broker: how many segments are registered.
    Status = 9,
    /// Broker → client: the answer to [`Opcode::Status`].
    StatusReply = 10,
    /// Client → broker: "I am the producer of this segment; keep this
    /// connection and push consumer doorbells down it".
    ///
    /// After the broker acknowledges this, the connection becomes push-only:
    /// the broker sends [`Opcode::RegisterDoorbell`] frames (with the
    /// descriptor attached) and the client never issues another request on
    /// it. Without this, a producer in a *different process from the broker*
    /// could never learn about a consumer's doorbell, because a
    /// [`crate::DoorbellRegistry`] holds descriptors and cannot live in
    /// shared memory.
    ClaimProducer = 11,
}

impl Opcode {
    /// Decode an opcode byte.
    ///
    /// # Errors
    ///
    /// [`ShmError::Protocol`] for an unknown opcode — the protocol is
    /// append-only, so an unknown value means a peer from the future, which
    /// this build must refuse rather than guess at.
    pub fn from_raw(raw: u8) -> ShmResult<Self> {
        Ok(match raw {
            1 => Self::Attach,
            2 => Self::AttachReply,
            3 => Self::Refused,
            4 => Self::RegisterDoorbell,
            5 => Self::Ack,
            6 => Self::Ping,
            7 => Self::Pong,
            8 => Self::CloseSegment,
            9 => Self::Status,
            10 => Self::StatusReply,
            11 => Self::ClaimProducer,
            other => {
                return Err(ShmError::protocol(format!("unknown broker opcode {other}")));
            }
        })
    }

    /// The name used in logs.
    ///
    /// # Examples
    ///
    /// ```
    /// # #[cfg(unix)] {
    /// use astrs_shm::protocol::Opcode;
    /// assert_eq!(Opcode::Attach.as_str(), "attach");
    /// # }
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Attach => "attach",
            Self::AttachReply => "attach-reply",
            Self::Refused => "refused",
            Self::RegisterDoorbell => "register-doorbell",
            Self::Ack => "ack",
            Self::Ping => "ping",
            Self::Pong => "pong",
            Self::CloseSegment => "close-segment",
            Self::Status => "status",
            Self::StatusReply => "status-reply",
            Self::ClaimProducer => "claim-producer",
        }
    }

    /// How many descriptors a frame with this opcode is expected to carry.
    #[must_use]
    pub const fn expected_fds(self) -> usize {
        match self {
            Self::AttachReply | Self::RegisterDoorbell => 1,
            _ => 0,
        }
    }
}

/// A framed broker message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerFrame {
    /// The opcode.
    pub opcode: Opcode,
    /// The payload bytes.
    pub payload: Vec<u8>,
}

impl BrokerFrame {
    /// Build a frame.
    #[must_use]
    pub const fn new(opcode: Opcode, payload: Vec<u8>) -> Self {
        Self { opcode, payload }
    }

    /// Build a frame with no payload.
    ///
    /// The wire always carries at least one byte of ordinary data because
    /// the header is twelve bytes, so an empty payload is still safe for
    /// `SCM_RIGHTS` — see [`crate::fdpass`].
    #[must_use]
    pub const fn empty(opcode: Opcode) -> Self {
        Self {
            opcode,
            payload: Vec::new(),
        }
    }

    /// Build a refusal carrying a human-readable reason.
    #[must_use]
    pub fn refused(reason: impl AsRef<str>) -> Self {
        let mut bytes = reason.as_ref().as_bytes().to_vec();
        bytes.truncate(MAX_BROKER_PAYLOAD);
        Self::new(Opcode::Refused, bytes)
    }

    /// Serialise the frame, header included.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(BROKER_HEADER_LEN + self.payload.len());
        bytes.extend_from_slice(&BROKER_MAGIC);
        bytes.push(BROKER_VERSION);
        bytes.push(self.opcode as u8);
        bytes.extend_from_slice(&0u16.to_le_bytes());
        let len = u32::try_from(self.payload.len()).unwrap_or(u32::MAX);
        bytes.extend_from_slice(&len.to_le_bytes());
        bytes.extend_from_slice(&self.payload);
        bytes
    }

    /// Decode a frame header.
    ///
    /// Returns the opcode and the payload length that follows.
    ///
    /// # Errors
    ///
    /// [`ShmError::Protocol`] for a short header, a bad magic, an unknown
    /// version or opcode, or a payload length above
    /// [`MAX_BROKER_PAYLOAD`].
    pub fn decode_header(bytes: &[u8]) -> ShmResult<(Opcode, usize)> {
        if bytes.len() < BROKER_HEADER_LEN {
            return Err(ShmError::protocol(format!(
                "broker header is {} bytes, expected {BROKER_HEADER_LEN}",
                bytes.len()
            )));
        }
        if bytes[..4] != BROKER_MAGIC {
            return Err(ShmError::protocol("broker frame magic mismatch"));
        }
        if bytes[4] != BROKER_VERSION {
            return Err(ShmError::protocol(format!(
                "broker protocol version {} is not supported (this build speaks {BROKER_VERSION})",
                bytes[4]
            )));
        }
        let opcode = Opcode::from_raw(bytes[5])?;
        let len = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
        if len > MAX_BROKER_PAYLOAD {
            return Err(ShmError::protocol(format!(
                "broker payload of {len} bytes exceeds the {MAX_BROKER_PAYLOAD}-byte limit"
            )));
        }
        Ok((opcode, len))
    }

    /// The payload interpreted as a refusal reason.
    #[must_use]
    pub fn reason(&self) -> String {
        String::from_utf8_lossy(&self.payload).into_owned()
    }
}

/// "Give me the descriptor for the segment with this key digest and
/// generation."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachRequest {
    /// The 128-bit digest of the segment key.
    pub key_digest: u128,
    /// The generation the client expects.
    pub generation: u64,
}

impl AttachRequest {
    /// The encoded size, in bytes.
    pub const ENCODED_LEN: usize = 24;

    /// Serialise.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(Self::ENCODED_LEN);
        bytes.extend_from_slice(&self.key_digest.to_le_bytes());
        bytes.extend_from_slice(&self.generation.to_le_bytes());
        bytes
    }

    /// Deserialise.
    ///
    /// # Errors
    ///
    /// [`ShmError::Protocol`] for a payload of the wrong length.
    pub fn decode(bytes: &[u8]) -> ShmResult<Self> {
        let (digest, rest) = take_u128(bytes, "attach request")?;
        let (generation, rest) = take_u64(rest, "attach request")?;
        expect_empty(rest, "attach request")?;
        Ok(Self {
            key_digest: digest,
            generation,
        })
    }
}

/// "Here is the descriptor; it maps this key digest at this generation."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachReply {
    /// The digest the segment actually carries.
    pub key_digest: u128,
    /// The generation the segment actually carries.
    pub generation: u64,
    /// The segment's total mapped length.
    pub total_len: u64,
}

impl AttachReply {
    /// The encoded size, in bytes.
    pub const ENCODED_LEN: usize = 32;

    /// Serialise.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(Self::ENCODED_LEN);
        bytes.extend_from_slice(&self.key_digest.to_le_bytes());
        bytes.extend_from_slice(&self.generation.to_le_bytes());
        bytes.extend_from_slice(&self.total_len.to_le_bytes());
        bytes
    }

    /// Deserialise.
    ///
    /// # Errors
    ///
    /// [`ShmError::Protocol`] for a payload of the wrong length.
    pub fn decode(bytes: &[u8]) -> ShmResult<Self> {
        let (digest, rest) = take_u128(bytes, "attach reply")?;
        let (generation, rest) = take_u64(rest, "attach reply")?;
        let (total_len, rest) = take_u64(rest, "attach reply")?;
        expect_empty(rest, "attach reply")?;
        Ok(Self {
            key_digest: digest,
            generation,
            total_len,
        })
    }
}

/// "Wire this doorbell descriptor to the producer of that segment."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DoorbellRegistration {
    /// The segment's key digest.
    pub key_digest: u128,
    /// The generation the client is attached to.
    pub generation: u64,
    /// The consumer-table index the client occupies.
    pub consumer_index: u32,
    /// The drop token proving the client owns that entry.
    pub token: u32,
}

impl DoorbellRegistration {
    /// The encoded size, in bytes.
    pub const ENCODED_LEN: usize = 32;

    /// Serialise.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(Self::ENCODED_LEN);
        bytes.extend_from_slice(&self.key_digest.to_le_bytes());
        bytes.extend_from_slice(&self.generation.to_le_bytes());
        bytes.extend_from_slice(&self.consumer_index.to_le_bytes());
        bytes.extend_from_slice(&self.token.to_le_bytes());
        bytes
    }

    /// Deserialise.
    ///
    /// # Errors
    ///
    /// [`ShmError::Protocol`] for a payload of the wrong length.
    pub fn decode(bytes: &[u8]) -> ShmResult<Self> {
        let (digest, rest) = take_u128(bytes, "doorbell registration")?;
        let (generation, rest) = take_u64(rest, "doorbell registration")?;
        let (consumer_index, rest) = take_u32(rest, "doorbell registration")?;
        let (token, rest) = take_u32(rest, "doorbell registration")?;
        expect_empty(rest, "doorbell registration")?;
        Ok(Self {
            key_digest: digest,
            generation,
            consumer_index,
            token,
        })
    }
}

/// "I am the producer of this segment, and this is my pid."
///
/// The pid matters as much as the claim: §6.3 has the daemon create a
/// segment *before* spawning the node that will write into it, so the pid
/// stamped at creation is the daemon's. Until the real producer says who it
/// is, the broker's liveness watch is aimed at itself and would never fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProducerClaim {
    /// The segment's key digest.
    pub key_digest: u128,
    /// The generation being claimed.
    pub generation: u64,
    /// The claiming process's id.
    pub pid: i64,
}

impl ProducerClaim {
    /// The encoded size, in bytes.
    pub const ENCODED_LEN: usize = 32;

    /// Serialise.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(Self::ENCODED_LEN);
        bytes.extend_from_slice(&self.key_digest.to_le_bytes());
        bytes.extend_from_slice(&self.generation.to_le_bytes());
        bytes.extend_from_slice(&self.pid.to_le_bytes());
        bytes
    }

    /// Deserialise.
    ///
    /// # Errors
    ///
    /// [`ShmError::Protocol`] for a payload of the wrong length.
    pub fn decode(bytes: &[u8]) -> ShmResult<Self> {
        let (digest, rest) = take_u128(bytes, "producer claim")?;
        let (generation, rest) = take_u64(rest, "producer claim")?;
        let (pid, rest) = take_u64(rest, "producer claim")?;
        expect_empty(rest, "producer claim")?;
        Ok(Self {
            key_digest: digest,
            generation,
            pid: pid as i64,
        })
    }
}

/// The broker's answer to [`Opcode::Status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StatusReply {
    /// How many segments the broker currently holds.
    pub segments: u32,
    /// How many of those have been marked closed.
    pub closed: u32,
    /// How many consumers are attached across all of them.
    pub consumers: u32,
}

impl StatusReply {
    /// The encoded size, in bytes.
    pub const ENCODED_LEN: usize = 12;

    /// Serialise.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(Self::ENCODED_LEN);
        bytes.extend_from_slice(&self.segments.to_le_bytes());
        bytes.extend_from_slice(&self.closed.to_le_bytes());
        bytes.extend_from_slice(&self.consumers.to_le_bytes());
        bytes
    }

    /// Deserialise.
    ///
    /// # Errors
    ///
    /// [`ShmError::Protocol`] for a payload of the wrong length.
    pub fn decode(bytes: &[u8]) -> ShmResult<Self> {
        let (segments, rest) = take_u32(bytes, "status reply")?;
        let (closed, rest) = take_u32(rest, "status reply")?;
        let (consumers, rest) = take_u32(rest, "status reply")?;
        expect_empty(rest, "status reply")?;
        Ok(Self {
            segments,
            closed,
            consumers,
        })
    }
}

fn take_u32<'a>(bytes: &'a [u8], what: &str) -> ShmResult<(u32, &'a [u8])> {
    let (head, rest) = split(bytes, 4, what)?;
    let mut buffer = [0u8; 4];
    buffer.copy_from_slice(head);
    Ok((u32::from_le_bytes(buffer), rest))
}

fn take_u64<'a>(bytes: &'a [u8], what: &str) -> ShmResult<(u64, &'a [u8])> {
    let (head, rest) = split(bytes, 8, what)?;
    let mut buffer = [0u8; 8];
    buffer.copy_from_slice(head);
    Ok((u64::from_le_bytes(buffer), rest))
}

fn take_u128<'a>(bytes: &'a [u8], what: &str) -> ShmResult<(u128, &'a [u8])> {
    let (head, rest) = split(bytes, 16, what)?;
    let mut buffer = [0u8; 16];
    buffer.copy_from_slice(head);
    Ok((u128::from_le_bytes(buffer), rest))
}

fn split<'a>(bytes: &'a [u8], len: usize, what: &str) -> ShmResult<(&'a [u8], &'a [u8])> {
    if bytes.len() < len {
        return Err(ShmError::protocol(format!(
            "{what} is truncated: needed {len} more bytes, {} remain",
            bytes.len()
        )));
    }
    Ok(bytes.split_at(len))
}

fn expect_empty(bytes: &[u8], what: &str) -> ShmResult<()> {
    if bytes.is_empty() {
        Ok(())
    } else {
        Err(ShmError::protocol(format!(
            "{what} has {} trailing bytes",
            bytes.len()
        )))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn frames_round_trip() {
        let frame = BrokerFrame::new(Opcode::Attach, vec![1, 2, 3]);
        let bytes = frame.encode();
        assert_eq!(bytes.len(), BROKER_HEADER_LEN + 3);
        let (opcode, len) = BrokerFrame::decode_header(&bytes).unwrap();
        assert_eq!(opcode, Opcode::Attach);
        assert_eq!(len, 3);
        assert_eq!(&bytes[BROKER_HEADER_LEN..], &[1, 2, 3]);
    }

    #[test]
    fn empty_frames_still_carry_a_full_header() {
        let bytes = BrokerFrame::empty(Opcode::Ping).encode();
        assert_eq!(bytes.len(), BROKER_HEADER_LEN);
        let (opcode, len) = BrokerFrame::decode_header(&bytes).unwrap();
        assert_eq!(opcode, Opcode::Ping);
        assert_eq!(len, 0);
    }

    #[test]
    fn hostile_headers_are_refused() {
        let good = BrokerFrame::empty(Opcode::Ping).encode();

        assert!(BrokerFrame::decode_header(&good[..11]).is_err());

        let mut bad = good.clone();
        bad[0] = b'X';
        assert!(BrokerFrame::decode_header(&bad).is_err());

        let mut bad = good.clone();
        bad[4] = BROKER_VERSION + 1;
        assert!(BrokerFrame::decode_header(&bad).is_err());

        let mut bad = good.clone();
        bad[5] = 200;
        assert!(BrokerFrame::decode_header(&bad).is_err());

        let mut bad = good;
        bad[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(BrokerFrame::decode_header(&bad).is_err());
    }

    #[test]
    fn every_opcode_decodes_and_names_itself() {
        let opcodes = [
            Opcode::Attach,
            Opcode::AttachReply,
            Opcode::Refused,
            Opcode::RegisterDoorbell,
            Opcode::Ack,
            Opcode::Ping,
            Opcode::Pong,
            Opcode::CloseSegment,
            Opcode::Status,
            Opcode::StatusReply,
            Opcode::ClaimProducer,
        ];
        for opcode in opcodes {
            assert_eq!(Opcode::from_raw(opcode as u8).unwrap(), opcode);
            assert!(!opcode.as_str().is_empty());
        }
        assert_eq!(Opcode::AttachReply.expected_fds(), 1);
        assert_eq!(Opcode::RegisterDoorbell.expected_fds(), 1);
        assert_eq!(Opcode::Ping.expected_fds(), 0);
        assert!(Opcode::from_raw(0).is_err());
        assert!(Opcode::from_raw(200).is_err());
    }

    #[test]
    fn attach_request_round_trips_and_rejects_malformed_payloads() {
        let request = AttachRequest {
            key_digest: 0x0123_4567_89ab_cdef_0123_4567_89ab_cdef,
            generation: 9,
        };
        let bytes = request.encode();
        assert_eq!(bytes.len(), AttachRequest::ENCODED_LEN);
        assert_eq!(AttachRequest::decode(&bytes).unwrap(), request);
        assert!(AttachRequest::decode(&bytes[..10]).is_err());
        let mut long = bytes;
        long.push(0);
        assert!(AttachRequest::decode(&long).is_err());
    }

    #[test]
    fn attach_reply_round_trips() {
        let reply = AttachReply {
            key_digest: 7,
            generation: 3,
            total_len: 65_536,
        };
        let bytes = reply.encode();
        assert_eq!(bytes.len(), AttachReply::ENCODED_LEN);
        assert_eq!(AttachReply::decode(&bytes).unwrap(), reply);
        assert!(AttachReply::decode(&[]).is_err());
    }

    #[test]
    fn doorbell_registration_round_trips() {
        let registration = DoorbellRegistration {
            key_digest: 11,
            generation: 2,
            consumer_index: 5,
            token: 17,
        };
        let bytes = registration.encode();
        assert_eq!(bytes.len(), DoorbellRegistration::ENCODED_LEN);
        assert_eq!(DoorbellRegistration::decode(&bytes).unwrap(), registration);
        assert!(DoorbellRegistration::decode(&bytes[..20]).is_err());
    }

    #[test]
    fn producer_claims_round_trip_including_a_negative_pid() {
        let claim = ProducerClaim {
            key_digest: 0xfeed_face,
            generation: 4,
            pid: 90_210,
        };
        let bytes = claim.encode();
        assert_eq!(bytes.len(), ProducerClaim::ENCODED_LEN);
        assert_eq!(ProducerClaim::decode(&bytes).unwrap(), claim);

        // A pid is an `i64` on the wire; the sign must survive.
        let negative = ProducerClaim { pid: -7, ..claim };
        assert_eq!(ProducerClaim::decode(&negative.encode()).unwrap(), negative);
        assert!(ProducerClaim::decode(&bytes[..16]).is_err());
    }

    #[test]
    fn status_reply_round_trips() {
        let status = StatusReply {
            segments: 3,
            closed: 1,
            consumers: 4,
        };
        let bytes = status.encode();
        assert_eq!(bytes.len(), StatusReply::ENCODED_LEN);
        assert_eq!(StatusReply::decode(&bytes).unwrap(), status);
        assert_eq!(StatusReply::default().segments, 0);
    }

    #[test]
    fn refusals_carry_a_readable_reason_and_are_bounded() {
        let frame = BrokerFrame::refused("no such segment");
        assert_eq!(frame.opcode, Opcode::Refused);
        assert_eq!(frame.reason(), "no such segment");

        let huge = "x".repeat(MAX_BROKER_PAYLOAD * 2);
        let frame = BrokerFrame::refused(huge);
        assert_eq!(frame.payload.len(), MAX_BROKER_PAYLOAD);
        let bytes = frame.encode();
        assert!(BrokerFrame::decode_header(&bytes).is_ok());
    }
}
