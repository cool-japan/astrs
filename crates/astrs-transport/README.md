# astrs-transport

UDS, TCP and QUIC connection abstraction with backpressure and reconnect for
AstRS.

One connection abstraction over three substrates, all speaking
`astrs-wire`'s framing: UDS (node ↔ daemon), TCP (fallback everywhere,
mandatory checksum) and QUIC (daemon ↔ daemon / daemon ↔ coordinator,
feature `quic`, currently non-default — see the crate's own "QUIC status"
docs for why). Everything above the socket is shared: the frame codec, the
`Hello`/`Welcome` handshake, the `ASTRS-MUX/1` route multiplexer (per-route
logical streams and flow control), the lz4/zstd compression container with
a decompression-bomb guard, and per-connection/per-route counters. A
backend contributes only a byte stream and a `PeerIdentity`, which is what
makes "the TCP fallback has identical framing" a property of the code
rather than a promise in a document. Every `TransportError` is classified —
fatal or not, retryable or not, whose fault it was — so the reconnect
supervisor and the daemon's metrics can act on it without matching on
variants.

## Example

```rust
use astrs_transport::{
    Connection, HandshakeParams, LocalIdentity, TransportAddr, TransportConfig,
};
use astrs_wire::{AuthToken, FrameKind, Role};

# async fn example() -> Result<(), astrs_transport::TransportError> {
let addr: TransportAddr = "tcp:10.0.0.4:7407".parse()?;
let config = TransportConfig::new();
let params = HandshakeParams::from_config(
    &config,
    LocalIdentity::new(Role::Peer).with_label("daemon-a"),
    AuthToken::from_bytes([7; 32]),
    addr.requires_crc(),
);

let (peer, mut channels) = astrs_transport::backend::connect(&addr, &config, &params).await?;
peer.open_control().send(FrameKind::PeerEvent, b"hello").await?;

let camera = peer.open_route(b"camera->detector")?;
camera.sender().send(FrameKind::Data, b"an arrow batch").await?;
# Ok(())
# }
```

See the [crate documentation](https://docs.rs/astrs-transport) for the
cross-host plane and the route-establishment handshake.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
