# astrs-wire

AstRS control-plane protocol types, framed codec and version negotiation.

This crate owns the single normative serialization surface of AstRS: the
framed codec (`magic "AS" | ver | flags | kind | len:u32 | payload |
crc32c`, oxicode-encoded, with optional lz4/zstd compression flags — this
crate negotiates and carries them, but the compression itself is applied
by `astrs-transport`, never by this crate), the message families for
every leg — `ControlRequest`/`ControlReply` (CLI ↔ coordinator),
`CoordinatorEvent`/`DaemonEvent` (coordinator ↔ daemon),
`NodeRequest`/`NodeEvent` (daemon ↔ node) and `PeerEvent` (daemon ↔ daemon)
— `Hello`/`Welcome`/`Refused` handshake and version negotiation, the
`ASTRS_NODE_CONFIG` handshake blob a daemon hands a spawned node, and the
append-only compatibility contract: every wire enum is `#[non_exhaustive]`,
encoded by stable variant index, extended only at the tail, and frozen by
protocol snapshot tests.

Every other crate in the workspace builds on this one for its wire
vocabulary — `astrs-transport`, `astrs-daemon`, `astrs-node-api` and
`astrs-coordinator` all depend on it directly rather than redefining
message shapes of their own; `DataId`, `NodeId`, `DataflowId` and
`Metadata` are the identifiers and envelope every layer above this one
passes around.

See the [crate documentation](https://docs.rs/astrs-wire) for the full frame
format, handshake sequence and message family reference.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
