# astrs-shm

Crash-safe POSIX shared-memory rings powering the AstRS zero-copy same-host
data plane.

One shared-memory ring per `(producer node, output port, generation)`,
written by exactly one producer and read by any number of consumers, with
no copy on the fast path and a reclamation protocol that survives any
participant being `kill -9`'d. Implements the full segment layout, the SPMC
ring, generation stamps, the drop-token reclamation protocol, doorbell
wakeups (eventfd/pipe), producer/consumer liveness (pidfd/kqueue), the pool
allocator, and the descriptor broker (`SCM_RIGHTS` fd passing) the daemon
uses to hand segments out. The ring's *logic* — geometry, validation, the
slot state machine, the consumer table — is platform-independent and
unit-tested everywhere, Windows included; only the syscalls are gated
behind `#[cfg(unix)]`, so a Windows port is a few primitives away rather
than a rewrite.

## Example

```rust
# #[cfg(unix)] {
use std::sync::Arc;
use astrs_shm::{AttachOptions, Consumer, OverflowPolicy, Producer, Segment, SegmentConfig, SegmentKey};
use astrs_wire::DataflowId;

let key = SegmentKey::from_parts(DataflowId::generate(), "camera", "image", 1)?;
let config = SegmentConfig::new(16, 64 * 1024)?.with_overflow(OverflowPolicy::Block);
let segment = Segment::create_shared(key, config)?;

let mut producer = Producer::new(Arc::clone(&segment))?;
let mut consumer = Consumer::attach(Arc::clone(&segment), AttachOptions::default())?;

let mut window = producer.allocate(6)?;
window.as_mut_slice().copy_from_slice(b"frame0");
window.commit(b"hlc")?;

let sample = consumer.try_next()?;
assert_eq!(sample.payload(), b"frame0");
# }
# Ok::<(), Box<dyn std::error::Error>>(())
```

See the [crate documentation](https://docs.rs/astrs-shm) for the segment
layout, memory ordering rules and the slow-start route-upgrade handshake that
hands a node one of these rings.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
