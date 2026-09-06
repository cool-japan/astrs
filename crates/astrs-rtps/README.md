# astrs-rtps

A from-scratch RTPS 2.3 stack on tokio UDP: discovery, reliability, QoS and
fragmentation.

Wire-level ROS 2 citizenship with no ROS installation and no `rustdds`
(and therefore no `mio 0.6`) anywhere in the tree. Built in two halves that
meet at a module boundary: the **message model** (`structure`, `messages`,
`error`) is a pure function of octets — value types, headers, every
submessage (`DATA`/`DATA_FRAG`/`HEARTBEAT`/`ACKNACK`/`GAP`/`NACK_FRAG`/the
`INFO_*` family), no state, no clock, no socket — and the **behavior**
half (`behavior`, `discovery`) builds the state machines on top of it
without editing it: SPDP/SEDP discovery, stateless best-effort and
stateful reliable writers/readers, HEARTBEAT/ACKNACK cadence, fragmentation
and reassembly, QoS, WLP liveliness, and the UDPv4 transport. Only the
participant/handle layer is asynchronous — writers, readers, proxies,
caches, the fragmenter and the discovery database are synchronous functions
of `(state, input, time)`, so the entire reliability protocol is testable
without a runtime. Four rules the crate never bends: alignment is checked,
`octetsToInlineQos` may grow but never shrink, impossible flag combinations
refuse to decode, and every declared length is checked before anything is
allocated.

## DDS-Security: what is here, and what is not

The `security` module implements **submessage protection** from
DDS-Security 1.1 — `SEC_PREFIX` / `SEC_BODY` / `SEC_POSTFIX` on the
specification's wire format, `AES128`/`AES256` in `GCM` (encrypt) and `GMAC`
(sign), per-session keys, and a sliding anti-replay window. Every primitive
comes from `oxicrypto`; nothing cryptographic is implemented in this crate.

Keying is a **pre-shared key** configured on both endpoints. The
authentication (PKI handshake), access-control (governance and permissions
XML), logging and data-tagging plugins are **not implemented**, whole-message
(`SRTPS_*`) and serialized-payload protection are **not implemented**, and
there is no identity or authorisation: every peer holding the key may both
publish and subscribe. **This does not interoperate with a PKI-based
DDS-Security stack** — the wire format matches, the keys do not, because the
handshake that would agree them does not happen. `KeyMaterial::from_parts`
is the seam an authenticated key agreement would plug into.

Two deliberate, documented deviations, both because the specification's
choice is weaker: the additional authenticated data binds the crypto header
(the specification leaves it unauthenticated for the GCM kinds;
`AadBinding::SpecEmpty` is one field away and is tested), and key material is
separated by topic so one pre-shared key across a deployment is not one key
across its topics.

An endpoint says `{ psk, protection: none | sign | encrypt }`. With
`protection: none` — the default — the octets on the wire are identical to a
build without the module, which the test suite asserts rather than assumes.

Three limits worth knowing before deploying it. **Metatraffic is in the
clear**: SPDP and SEDP are never protected, so topic names, type names and
QoS stay visible to anyone on the network and only the user data on those
topics is protected — hiding discovery is what whole-message (`SRTPS_*`)
protection is for. The RTPS header is **outside** the authenticated data, so
a replay under a spoofed participant prefix is absorbed by the reader's
sequence-number handling rather than refused by the transform. And a
plaintext submessage addressed to `ENTITYID_UNKNOWN` is refused rather than
routed on any participant with a protected endpoint. The `security` module
documentation states all three, with the reasoning and the cost of closing
each.

Wiring it up: `ParticipantConfig::with_security(EndpointSecurity)` sets a
participant-wide default for user endpoints, and
`Participant::create_secure_writer` / `create_secure_reader` take an
`EndpointSecurity` per endpoint. `ProtectionKind::parse` accepts exactly
`"none"`, `"sign"` and `"encrypt"`, and `Psk::from_hex` reads the key a
configuration file would carry — those two are what a manifest or
ros2-endpoint field maps onto.

## Example

```rust
use astrs_rtps::messages::{Data, DataPayload, Message, SerializedPayload};
use astrs_rtps::structure::{
    ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER, ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER,
    GuidPrefix, SequenceNumber, VendorId, port,
};

let prefix = GuidPrefix::vendor_scoped(VendorId::ASTRS, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
let announcement = Message::from_participant(prefix).with(Data::new(
    ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER,
    ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER,
    SequenceNumber::FIRST,
    DataPayload::Data(SerializedPayload::from_cdr(&0_u32)?),
));

let datagram = announcement.encode()?;
assert_eq!(Message::decode(&datagram)?, announcement);

let group = port::default_multicast_locator(0).expect("domain 0 fits");
assert_eq!(group.to_string(), "239.255.0.1:7400");
# Ok::<(), astrs_rtps::RtpsError>(())
```

See the [crate documentation](https://docs.rs/astrs-rtps) for the RTPS 2.3
design, including the Humble/Jazzy GID switch.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
