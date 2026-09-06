# astrs-scheduler

Input queues, hierarchical timer wheel, deadline monitor and priority lanes
for AstRS.

This is the layer that decides what a node's merged event loop sees next,
and when. `astrs-daemon` and `astrs-node-api` are its two intended
consumers — the daemon multiplexing many nodes' routes, the node API
multiplexing one node's own inputs — and both reuse the same mechanism,
generic over the message payload. Five independent pieces, composed by the
caller rather than by this crate: `InputQueue` (bounded, policy-driven,
eviction-immune for Stop-class control events and correlated messages),
`EventMux` (the prioritized, fair, multi-input merged event loop — control
strictly pre-empts data, round robin within a lane), `TimerWheel` (the
hierarchical wheel serving every `astrs/timer/*` subscription, drift-free,
with a configurable missed-tick policy), `DeadlineMonitor` (per-input
input-to-output latency budgets, token-based so pipelined measurements
never clobber each other), and `IdleWatchdog` (per-input silence detection
— "declare this input closed if nothing arrives for this long", distinct
from a deadline's "was this span slow"). Nothing here logs or emits a
metric on its own; every condition worth attention is an ordinary return
value for the caller to route to its own observability stack.

## Example

```rust
use astrs_scheduler::{EventMux, Envelope};
use astrs_wire::{DataId, PriorityLane, QueuePolicy};

// One mux per node; one input per manifest `inputs:` entry.
let mux: EventMux<Envelope<u32>> = EventMux::new();
let frames = mux.register_input(
    DataId::new("frames")?, 10, QueuePolicy::DropOldest, PriorityLane::Data,
)?;
let status = mux.register_input(
    DataId::new("status")?, 4, QueuePolicy::DropOldest, PriorityLane::Control,
)?;

frames.push(Envelope::new(42)); // an ordinary data message
status.push(Envelope { payload: 0, metadata: None, stop: true }); // Stop-class

// The control lane is served first, regardless of arrival order.
let (first, event) = mux.try_recv().expect("something is queued");
assert_eq!(first, DataId::new("status")?);
assert!(event.stop);
# Ok::<(), Box<dyn std::error::Error>>(())
```

`TimerWheel`, `DeadlineMonitor` and `IdleWatchdog` compose the same way —
see the crate docs for the full walkthrough.

See the [crate documentation](https://docs.rs/astrs-scheduler) for the
scheduling and real-time design.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
