//! RTPS 2.3 in pure Rust, on tokio.
//!
//! Wire-level ROS 2 citizenship with no ROS installation and no `rustdds`
//! (and therefore no `mio 0.6`) anywhere in the tree (blueprint §10.2). The
//! crate is built in two halves that meet at a module boundary, and this page
//! documents the boundary as well as the half that is here.
//!
//! # The two halves
//!
//! | Half | Modules | What it owns |
//! |---|---|---|
//! | **Message model** | [`structure`], [`messages`], [`error`] | The wire format: value types, headers, every submessage, encode and decode. No state, no clock, no socket. |
//! | **Behavior** | [`behavior`], [`discovery`], [`security`] | The state machines: SPDP and SEDP discovery, stateless and stateful writers and readers, HEARTBEAT/ACKNACK cadence, fragmentation and reassembly, QoS, WLP liveliness, DDS-Security submessage protection, the UDPv4 transport on tokio. |
//!
//! The split is deliberate and load-bearing. Everything in the message model
//! is a pure function of octets: `decode` takes a `&[u8]` and returns a
//! value, `encode` takes a value and returns octets, and no test of either
//! needs a runtime, a port or a sleep. The behavior half builds on it in
//! `src/behavior/` and `src/discovery/` **without editing anything under
//! `src/structure/` or `src/messages/`** — it consumes these types, and where
//! it needs new ones it defines them in its own modules.
//!
//! The behavior half then repeats the trick one level up. Only
//! [`behavior::participant`] and [`behavior::handle`] are asynchronous;
//! the writers, the readers, the proxies, the caches, the fragmenter and the
//! discovery database are all synchronous and take `now` as an argument. So
//! the entire reliability protocol — the part that is genuinely hard to get
//! right — is a pure function of (state, input, time), and its tests need no
//! runtime either.
//!
//! Errors follow the same rule. [`RtpsError`] is the *wire format* taxonomy
//! and nothing else; the behavior half defines its own error types for
//! protocol state — an unmatched reader, an expired lease, a socket that
//! would not bind — and converts through `#[from] RtpsError` rather than
//! extending this enum.
//!
//! # What the message model covers
//!
//! - **[`structure`]** — the §9.3 and §9.4.2 value types: [`ProtocolVersion`]
//!   and [`VendorId`]; [`GuidPrefix`], [`EntityId`] and [`Guid`] with the
//!   standard well-known entity ids; [`SequenceNumber`] and
//!   [`SequenceNumberSet`]; [`FragmentNumber`] and [`FragmentNumberSet`];
//!   [`Locator`] and [`LocatorList`] for UDPv4 and loopback; [`Time`],
//!   [`Duration`] and the DDS-flavoured [`DdsDuration`]; and the §9.6.1.1
//!   port mapping in [`structure::port`].
//! - **[`messages`]** — the §8.3 grammar: the [`Header`], the
//!   [`SubmessageHeader`] with the full `octetsToNextHeader` rules, and
//!   `DATA`, `DATA_FRAG`, `HEARTBEAT`, `HEARTBEAT_FRAG`, `ACKNACK`,
//!   `NACK_FRAG`, `GAP`, `INFO_TS`, `INFO_DST`, `INFO_SRC`, `INFO_REPLY` and
//!   `PAD`, each with its flag semantics, its byte-order handling and its
//!   §8.3.7 validity clauses.
//! - **[`error`]** — [`RtpsError`], fine-grained enough that a dropped
//!   datagram's log line names the octet that broke.
//!
//! # Security, and the honest size of it
//!
//! [`security`] implements DDS-Security 1.1 **submessage protection** —
//! `SEC_PREFIX`/`SEC_BODY`/`SEC_POSTFIX`, AES-GCM and AES-GMAC, per-session
//! keys, anti-replay — on **pre-shared keys**. The authentication and
//! access-control plugins, whole-message protection and payload protection
//! are not implemented, and there is therefore **no interoperability with a
//! PKI-based DDS-Security stack**. The module's own documentation says so at
//! the top, names its two deliberate deviations from the specification, and
//! points at the seam a handshake would plug into. Every primitive comes from
//! `oxicrypto`; this crate implements none.
//!
//! An endpoint that says nothing about security is byte-identical to a build
//! without the module — asserted in `tests/security.rs`, not assumed.
//!
//! # Strictness
//!
//! Four rules this crate does not bend, because each turns a silent
//! misinterpretation into a typed error:
//!
//! 1. **Alignment is checked.** A submessage that would start at an offset
//!    that is not a multiple of four stops the parse (§8.3.3); an encoder
//!    asked to follow a body that is not a multiple of four refuses.
//! 2. **`octetsToInlineQos` may grow but not shrink.** A larger value is the
//!    §8.3.7.2.2 forward-compatibility escape and the extra octets are
//!    skipped; a smaller one would overlap `writerSN` and is rejected.
//! 3. **Impossible flag combinations do not decode.** A `DATA` with both `D`
//!    and `K` is refused rather than resolved by precedence.
//! 4. **Declared lengths are checked before anything is allocated.** A
//!    `numBits` above 256, a locator count larger than the datagram, a
//!    submessage count past [`messages::MAX_SUBMESSAGES`] — all rejected
//!    before the first `Vec` grows.
//!
//! And two rules it bends on purpose, because §8.6 requires it: an unknown
//! `submessageId` is skipped rather than fatal, and flag bits and trailing
//! body octets a later minor version added are preserved in
//! [`Extension`](messages::Extension) rather than rejected.
//!
//! # Golden packets, and two live participants
//!
//! `tests/golden_*.rs` carries the conformance vectors: an SPDP announcement,
//! a `DATA` with inline QoS, a HEARTBEAT/ACKNACK/GAP exchange, a `DATA_FRAG`
//! series and an `INFO_TS`+`DATA` pairing — every one derived octet by octet
//! from the OMG DDSI-RTPS field tables, with the derivation written out in
//! the test beside it. No fixture in this repository was captured from a C or
//! C++ DDS stack; cross-stack validation lives in a separate out-of-repo
//! project, per blueprint §18.
//!
//! `tests/e2e_*.rs` is the other half of §10.2's in-repo proof: two
//! independent [`Participant`]s in one process, on
//! real UDP sockets, completing SPDP and SEDP, exchanging reliable and
//! best-effort samples, surviving induced loss injected at a
//! [`DatagramSocket`](behavior::DatagramSocket) wrapper, and round-tripping a
//! one-megabyte fragmented payload. Every socket binds port 0 and every
//! deterministic assertion runs over unicast initial peers, because a
//! sandboxed host refuses `IP_ADD_MEMBERSHIP` — the multicast join is
//! implemented, probed, and its outcome asserted rather than skipped.
//!
//! # Example
//!
//! ```
//! use astrs_rtps::messages::{Data, DataPayload, Message, SerializedPayload};
//! use astrs_rtps::structure::{
//!     ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER, ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER,
//!     GuidPrefix, SequenceNumber, VendorId, port,
//! };
//!
//! let prefix = GuidPrefix::vendor_scoped(VendorId::ASTRS, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
//! let announcement = Message::from_participant(prefix).with(Data::new(
//!     ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER,
//!     ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER,
//!     SequenceNumber::FIRST,
//!     DataPayload::Data(SerializedPayload::from_cdr(&0_u32)?),
//! ));
//!
//! let datagram = announcement.encode()?;
//! assert_eq!(Message::decode(&datagram)?, announcement);
//!
//! // …and it goes to 239.255.0.1:7400 on domain 0.
//! let group = port::default_multicast_locator(0).expect("domain 0 fits");
//! assert_eq!(group.to_string(), "239.255.0.1:7400");
//! # Ok::<(), astrs_rtps::RtpsError>(())
//! ```

// Never called. The dependency exists solely so Cargo's feature unification
// forces `blake3/pure` (Rust-only) onto the copy `oxicrypto` pulls in —
// without it, blake3's build script compiles C SIMD kernels via `cc`, and the
// tree must stay C/C++-free in every feature combination and for every
// package-scoped build (blueprint §18.1). Same note as `astrs-wire`.
use blake3 as _;

pub mod behavior;
pub mod discovery;
pub mod error;
pub mod messages;
pub mod security;
pub mod structure;

pub use error::{FragmentDefect, RtpsError, RtpsResult, SetDefect};
pub use messages::{
    AckNack, Data, DataFrag, DataPayload, Gap, Header, Heartbeat, HeartbeatFrag, InfoDestination,
    InfoReply, InfoSource, InfoTimestamp, Message, MessageBuilder, NackFrag, Opaque, Pad,
    SerializedPayload, Submessage, SubmessageFlags, SubmessageHeader, SubmessageId,
};
pub use structure::{
    DdsDuration, Duration, EntityId, EntityKind, FragmentNumber, FragmentNumberSet, Guid,
    GuidPrefix, Locator, LocatorKind, LocatorList, ProtocolVersion, SequenceNumber,
    SequenceNumberSet, Time, VendorId,
};

pub use behavior::{
    BehaviorError, BehaviorResult, MulticastCapability, Participant, ParticipantConfig,
    ReaderHandle, RtpsReader, RtpsWriter, Sample, TopicKey, WriterHandle,
};
pub use discovery::{
    DiscoveryDb, DiscoveryEvent, ReaderQos, RosCompat, SpdpConfig, WriterQos, check_qos,
};
pub use security::{
    AadBinding, EndpointSecurity, KeyMaterial, ProtectionKind, Psk, SecurityContext, SecurityError,
    TransformationKind,
};
