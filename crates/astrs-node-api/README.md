# astrs-node-api

The AstRS node API: handshake, typed pub/sub, zero-copy allocation and
event streams.

The flagship user-facing surface: everything a robotics node needs to join a
dataflow, read its inputs and publish its outputs, and nothing it does not.
`Node::init_from_env`/`Node::builder`/`Node::init_testing` for startup — the
last spins an in-process `MockDaemon` speaking the real wire protocol, so a
node's whole lifecycle is a unit test with no daemon involved. `EventStream`
is both an `Iterator` and a `Stream`, so the same loop works synchronously or
under `async`, and fuses after `Stop`. Typed (`Output<T>`) and raw
(`RawOutput`) sending, built on `astrs-data`'s own columnar types with no
extra feature required — the non-default `arrow-interop` feature adds only
`send_arrow`, a bridge for a caller who already holds a value from the
upstream arrow-rs ecosystem — plus service/action/stream pattern helpers,
structured logging, and extension storage. The slow-start zero-copy handshake
is wired at both ends and a node writes no code for either: a
producer's `RouteUpgrade` opens a shared-memory producer transparently, and a
consumer's `InputRouteUpgrade` attaches the segment so `Event::Input`'s
payload is zero-copy — both directions fall back to the reliable daemon path
exactly as transparently on a downgrade.

## Example

```rust
use astrs_node_api::prelude::*;

fn main() -> Result<(), NodeError> {
    let (mut node, mut events) = Node::init_from_env()?;
    let mut echo = node.raw_output("echo")?;

    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, data, meta } if id == "frames" => {
                echo.send_bytes(data.to_vec(), meta.follow())?;
            }
            Event::Stop(_) => break,
            _ => {}
        }
    }
    Ok(())
}
```

See the workspace [README](https://github.com/cool-japan/astrs#writing-a-node)
for the full node API surface.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
