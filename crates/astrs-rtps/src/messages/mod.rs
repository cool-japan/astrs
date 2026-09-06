//! The RTPS message model: headers, submessages, and the octets they become.
//!
//! Everything an RTPS participant puts on a socket is a [`Message`] — a
//! twenty-octet [`Header`] naming the sender, then submessages. This module
//! is the whole of that grammar (OMG DDSI-RTPS 2.3 §8.3), and nothing more:
//! it has no state, no clock and no socket, so a datagram can be built,
//! encoded, decoded and compared in a unit test with no runtime at all.
//!
//! | Module | Contents |
//! |---|---|
//! | [`kind`] | [`SubmessageId`] — the thirteen assigned ids and the vendor range |
//! | [`flags`] | [`SubmessageFlags`], the per-kind bit names, and [`Extension`] |
//! | [`header`] | [`Header`], [`SubmessageHeader`], the `octetsToNextHeader` rules |
//! | [`payload`] | [`SerializedPayload`] — the opaque octets a sample is |
//! | [`data`] | [`Data`], [`DataFrag`], [`DataPayload`] |
//! | [`heartbeat`] | [`Heartbeat`], [`HeartbeatFrag`] |
//! | [`acknack`] | [`AckNack`], [`NackFrag`] |
//! | [`gap`] | [`Gap`] |
//! | [`info`] | [`InfoTimestamp`], [`InfoSource`], [`InfoDestination`], [`InfoReply`] |
//! | [`submessage`] | [`Submessage`], [`Pad`], [`Opaque`] |
//! | [`message`] | [`Message`], [`MessageBuilder`], [`SubmessageIter`] |
//!
//! # The shape of every submessage type
//!
//! Each one follows the same five-method contract, so the dispatch in
//! [`Submessage`] is mechanical and a new kind is a small, obvious addition:
//!
//! | Method | Meaning |
//! |---|---|
//! | `new(…)` | a little-endian submessage with no optional field set |
//! | `flags()` | the flags octet the current field values imply |
//! | `body_len()` | octets the body will occupy, header excluded |
//! | `write_body(&mut CdrWriter)` | the octets after the four-octet header |
//! | `read(&SubmessageHeader, &[u8])` | the inverse, given the header's flags |
//! | `validate()` | the §8.3.7 clauses, where the kind has any |
//!
//! The flags octet is *derived*, never stored: a `DATA` sets `Q` because it
//! has an `inlineQos`, and sets `D` because its [`DataPayload`] is a
//! `Data`. A state where the flag and the field disagree is unconstructible.
//! The bits a kind does not define are the exception — those are kept in
//! [`Extension`], along with any octets a later protocol version appended to
//! the body, so a submessage decoded from a future peer re-encodes to the
//! octets it arrived as.
//!
//! # Alignment and byte order
//!
//! A submessage body is plain CDR with no encapsulation header of its own,
//! its alignment origin at the first octet after the four-octet submessage
//! header, and its byte order taken from bit 0 of the flags. That is exactly
//! what `astrs-cdr`'s [`CdrReader::with_encoding`](astrs_cdr::CdrReader::with_encoding)
//! and [`CdrWriter::headerless`](astrs_cdr::CdrWriter::headerless) provide,
//! so this crate reuses `astrs-cdr`'s fuzz-hardened primitives — including
//! the length checks that reject a hostile count before anything is
//! allocated — instead of hand-rolling a cursor. [`body_encoding`] is the
//! one-line bridge.
//!
//! # A worked message
//!
//! ```
//! use astrs_rtps::messages::{Data, DataPayload, Header, Message, SerializedPayload,
//!     InfoTimestamp};
//! use astrs_rtps::structure::{EntityId, EntityKind, GuidPrefix, SequenceNumber, Time};
//!
//! let message = Message::from_participant(GuidPrefix::new([1; 12]))
//!     .with(InfoTimestamp::at(Time::new(1_700_000_000, 0)))
//!     .with(Data::new(
//!         EntityId::UNKNOWN,
//!         EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY),
//!         SequenceNumber::FIRST,
//!         DataPayload::Data(SerializedPayload::from_cdr(&1_234_i32)?),
//!     ));
//!
//! let datagram = message.encode()?;
//! assert_eq!(datagram.len(), 20 + (4 + 8) + (4 + 28));
//! assert_eq!(Message::decode(&datagram)?, message);
//! # Ok::<(), astrs_rtps::RtpsError>(())
//! ```

pub mod acknack;
pub mod data;
pub mod flags;
pub mod gap;
pub mod header;
pub mod heartbeat;
pub mod info;
pub mod kind;
pub mod message;
pub mod payload;
pub mod submessage;

pub use acknack::{ACKNACK_FIXED_LEN, AckNack, NACK_FRAG_FIXED_LEN, NackFrag};
pub use data::{
    DATA_FRAG_OCTETS_TO_INLINE_QOS, DATA_FRAG_PRELUDE_LEN, DATA_OCTETS_TO_INLINE_QOS,
    DATA_PRELUDE_LEN, Data, DataFrag, DataPayload, FragmentGeometry,
};
pub use flags::{Extension, SubmessageFlags, body_encoding};
pub use gap::{GAP_FIXED_LEN, Gap};
pub use header::{
    BodyExtent, HEADER_LEN, Header, MAX_OCTETS_TO_NEXT_HEADER, SUBMESSAGE_ALIGNMENT,
    SUBMESSAGE_HEADER_LEN, SubmessageHeader,
};
pub use heartbeat::{HEARTBEAT_BODY_LEN, HEARTBEAT_FRAG_BODY_LEN, Heartbeat, HeartbeatFrag};
pub use info::{
    INFO_DESTINATION_BODY_LEN, INFO_SOURCE_BODY_LEN, InfoDestination, InfoReply, InfoSource,
    InfoTimestamp,
};
pub use kind::SubmessageId;
pub use message::{
    DEFAULT_DATAGRAM_BUDGET, MAX_SUBMESSAGES, MAX_UDP_PAYLOAD, Message, MessageBuilder,
    SubmessageIter,
};
pub use payload::{SerializedPayload, inline_qos_encoding, read_inline_qos, relabel_inline_qos};
pub use submessage::{Opaque, Pad, Submessage};
