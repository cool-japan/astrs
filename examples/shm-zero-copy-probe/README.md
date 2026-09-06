# `shm-zero-copy-probe`

The **M1 zero-copy verification**: 4 MiB frames written straight
into a shared-memory ring and read in place by the consumer, with the numbers
printed rather than asserted in a comment.

```bash
cargo build -p shm-zero-copy-probe
astrs run examples/shm-zero-copy-probe/dataflow.yml
```

```
producer: zero-copy plane engaged after 42.7ms
zero-copy: ENGAGED
frames:    24 x 4194304 B = 96.0 MiB
segment:   <dataflow>/producer/frames/0 (generation 0, 8 slots)
addresses: 8 distinct / 8 slots, layout yes, in-mapping yes
identity:  sequences yes, fingerprints yes
```

## What it proves, and how

AstRS starts **every** output on the reliable daemon path and moves it
to shared memory only once the daemon has seen every same-host consumer attach
to the ring. So the producer waits — on an ordinary timer input, not in a sleep
loop — until `output.plane()` reports `Shm`, and only then allocates. Each
frame is generated *into* the ring slot `RawOutput::allocate` hands back: no
staging buffer, one pass over the bytes between `allocate` and `send`. A slot
the ring cannot give is skipped and counted, never quietly downgraded to the
heap, because counting a heap frame would inflate the very number the probe
exists to report.

The consumer proves the bytes were never copied, from facts it observes
itself — through `Payload::slot()`, which reports where in the ring a message
arrived:

1. **The address is where the ring's layout says it is** —
   `SlotLocation::is_at_layout_offset`, computed from the mapping base plus the
   layout's offset for that slot rather than from the pointer being checked, so
   it is a real check and not a restatement.
2. **The address is inside the mapping** — `SlotLocation::is_inside_mapping`.
   A heap copy is not.
3. **The addresses cycle** — at most `slot_count` distinct payload addresses
   over the whole run, and slot *n* is the same address every time it comes
   round. A copying path hands out a fresh buffer every time.
4. **The sequence stamped in the bytes matches the ring's own** — the producer
   writes `SampleMut::seq` into the payload before committing, so reading *N*
   out of the bytes and being told *N* by `SlotLocation::sequence` means this is
   that exact write.
5. **The fingerprint matches**, recomputed over the body while it is still
   mapped.

Raw virtual addresses are deliberately *not* compared across the two
processes: the same physical page is mapped wherever each `mmap` chose, so
equality would be meaningless and inequality would prove nothing.
Offset-within-mapping is the portable form of the same question.

The consumer writes all of it as JSON to `$PROBE_REPORT`, which is what the
M1 conformance test reads.

## The consumer no longer attaches by hand

**This example changed.** It used to open the segment itself: read the
producer's incarnation out of the `ready` announcement, build a `SegmentKey`,
dial the daemon's broker socket out of `ASTRS_NODE_CONFIG`, attach an
`astrs_shm::Consumer` and read the ring in a second loop beside the node's
event loop — a thick slab of plumbing, in an example whose subject is
supposed to be the *plane*, not the plumbing.

It did that because the frozen `daemon → node` family had no message for it:
`NodeEvent::RouteUpgrade` names an **output**, which is the producer's half of
the route upgrade, and a consumer needed to be told about an **input**. The
wire protocol is append-only, so the fix was a tail append —
`NodeEvent::InputRouteUpgrade` and
`NodeEvent::InputRouteDowngrade`, indices 15 and 16, with every frozen byte
before them unmoved (`crates/astrs-wire/tests/golden/protocol.frozen.snap`
proves it).

The daemon now sends `InputRouteUpgrade` as soon as a ring exists and every
consumer of that output is eligible for it, and `astrs-node-api` attaches on
its own. So the consumer is what a consumer should be:

```rust
while let Some(event) = events.recv() {
    if let Event::Input { id, data, .. } = event {
        // `data.is_zero_copy()` is true; `data.slot()` says which ring slot.
    }
}
```

There is no `astrs-shm` import in `src/bin/consumer.rs` any more, and the probe
asserts that: `attached_automatically` in the report is only true because
`Node::input_plane_stats().attaches` counted an attachment the *middleware*
made. The ordering is the daemon's, and it is forced — a consumer is told
first, because the producer is offered its upgrade only once the daemon has
*observed* every consumer in the segment's consumer table, which cannot happen
before one attaches.

### The `ready` announcement changed jobs

It used to carry the producer's incarnation. Now it is a **sub-threshold
message on an upgraded output**: eight bytes, far below the zero-copy
threshold, so it rides the daemon's control channel even after `ready` itself
has moved to the ring. That is the one case where an upgraded route still has
two carriers, and it is the case that used to lose messages silently — the
daemon skipped every consumer that was on a ring, whether or not the bytes had
gone through one.

The producer publishes one announcement *after* the first ring frame, sixteen
bytes carrying `ANNOUNCE_UPGRADED` behind the generation, and
`announcement_after_upgrade` in the report says whether it arrived. It is a
conjunct of `zero_copy_engaged`: a run whose large frames took the ring but
lost the small message behind them is not a working plane.

The marker matters. The consumer identifies that announcement by **content**,
not by when it turns up: `frames` and `ready` are two inputs, and which of two
inputs the event mux serves first is its business, not a fact a probe
may lean on. An earlier version inferred "this one came after the upgrade"
from "a frame has already been verified" and reported a false negative every
time the mux happened to serve `ready` late — a probe that measures the
scheduler when it means to measure the plane.

## Notes for readers

- `shm_pool_size: 41943040` on the producer. The default 8 MiB pool splits
  into eight 1 MiB slots, and a 4 MiB frame would not fit one — every frame
  would fall back to the heap. Sizing the pool for the payload is the one
  manifest knob this plane needs.
- The `frames` port is deliberately **untyped**. The probe measures the memory
  path, so its payload is a hand-rolled 128-byte header plus a generated body
  rather than a columnar message; the design principles keep raw ports available
  as long as they are explicit, and an absent `output_types` entry is exactly
  that.
- Ordering caveat, stated plainly: an output that publishes on *both* sides of
  the zero-copy threshold has two carriers with different latencies, so a large
  and
  a small message can be reordered relative to each other. An output whose
  payloads are uniformly sized — the normal case, since a port carries one type
  — stays on one carrier and stays in order. `frames` is uniform; `ready` is a
  separate port.
- Timings are printed, never asserted. A debug build folds 4 MiB per frame in
  software and a shared CI box schedules how it likes; the *structural*
  invariants above are what make the verdict, and they hold either way. Build
  with `--release` for numbers worth quoting.
- `PROBE_FRAMES`, `PROBE_FRAME_BYTES`, `PROBE_UPGRADE_TIMEOUT_MS` and
  `PROBE_REPORT` tune a run from the manifest's `env:` block.
