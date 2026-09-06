# `streaming-segments`

Chunked streaming of large payloads over the
`session_id`/`segment_id`/`seq`/`fin` pattern, verified byte-for-byte:

```bash
cargo build -p streaming-segments
astrs run examples/streaming-segments/dataflow.yml
cat "${TMPDIR:-/tmp}/astrs-streaming-segments-report.json"
```

```
  [streamer] ──chunks──► [collector]
```

## What this proves

`streamer` publishes six 256 KiB synthetic "segments" (a stand-in for a
point cloud or a keyframe), each split into 16 KiB chunks by
`Node::stream_segment` — the same call handles the `session_id`/
`segment_id`/`seq`/`fin` bookkeeping the pattern needs, so this example's
own code never numbers a chunk by hand. `collector` feeds every event to a
`StreamAssembler`, which reassembles each segment and hands back the
complete bytes. Those bytes are then checked against `segment_payload`, the
same pure, deterministic function `streamer` used to generate them — so a
byte the wire dropped, duplicated or reordered is caught even if the chunk
*count* still matched.

## Why the queues are deep and lossless

A stream chunk carries none of the correlation keys (`request_id`,
`goal_id`/`goal_status`) — those confer queue-eviction immunity, and
`session_id`/`segment_id`/`seq`/`fin` deliberately do not. A stream that
outruns its consumer is meant to drop, with the assembler reporting the gap
rather than silently reassembling the wrong bytes. This example chooses not
to exercise that path: both ends of the `chunks` edge use a deep queue and
`queue_policy: backpressure`, so a slow `collector` stalls `streamer`
instead of losing a chunk — `record-replay` makes the identical choice, for
the identical reason (its byte-for-byte claim would be untestable over a
lossy edge).

## Sizing, and what this example does *not* claim

Sixteen-kilobyte chunks sit comfortably above `astrs-shm`'s default
zero-copy threshold (4 KiB), so on a real same-host run they are
candidates for the shared-memory plane once the route upgrades. This
example does not assert that engagement — `shm-zero-copy-probe` already owns
that claim, with the introspection (`Payload::slot`, ring-address checks) it
takes to prove it honestly. `streaming-segments`'s own claim is narrower and
orthogonal: whatever plane the bytes travel on, chunked reassembly is
correct.

## Files

| File | What it is |
|---|---|
| `dataflow.yml` | the two-node graph |
| `src/bin/segment_streamer.rs` | splits each segment into chunks via `Node::stream_segment` |
| `src/bin/segment_collector.rs` | reassembles via `StreamAssembler` and verifies |
| `src/lib.rs` | the deterministic segment generator and `StreamTally`, all unit-tested |

| Environment variable | Default |
|---|---|
| `STREAMING_SEGMENTS_COUNT` | `6` (set by the manifest) |
| `STREAMING_SEGMENTS_REPORT` | `$TMPDIR/astrs-streaming-segments-report.json` |
