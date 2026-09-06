# `record-replay`

The recording and replay promise, made checkable:

> Replay: `astrs replay session.arec --speed 1.0 --into graph.yml` re-injects
> recorded outputs as sources — replacing any subset of nodes (test a new
> planner against last week's sensor data **byte-for-byte**).

```bash
cargo build -p record-replay -p astrs-cli -p astrs-replay-node
astrs run    examples/record-replay/dataflow.yml        # 1. record
astrs bag info "${TMPDIR:-/tmp}/astrs-record-replay-session.arec"

astrs replay "${TMPDIR:-/tmp}/astrs-record-replay-session.arec" \
      --into examples/record-replay/replay.yml > /tmp/replayed.yml
PATH="$PWD/target/debug:$PATH" astrs run /tmp/replayed.yml   # 2. replay
```

```
live:    astrs/timer ──► [sensor] ──frames──► [detector] ──detections──┐
                              └────frames─────────────────────────────┴──► [recorder] ──► session.arec

replay:  session.arec ──► [sensor := astrs-replay-node] ──frames──► [probe] ──► sequence.json
```

## What the two runs prove together

The live run writes an `.arec` session holding every payload that crossed
`sensor/frames`. The replay run is the *same* graph with two changes: the
sensor is served from that file instead of from a clock, and **the detector is
replaced by an assertion sink**. `tests/conformance/tests/m2_record_replay.rs`
then asserts a three-way byte equality:

| Sequence | Where it comes from |
|---|---|
| what the sensor generated | `record_replay::frame_payload(i)`, a pure function of the frame index |
| what the live run carried | the `.arec` entries for `sensor/frames` |
| what the replay delivered | the probe's `PayloadSequence` JSON |

All three must be the identical list of hex payloads, in the identical order.
Not a count, not a digest — the payloads themselves.

## What is deliberately *not* compared

Timestamps. `astrs-replay-node` stamps every republished message with a
**fresh** HLC from its own clock (replay is a new live event, not a literal
replica of history) and carries the rest of the metadata through unchanged.
Payload bytes are reproducible; the clock is not.

## Three details worth copying

- **Every edge is lossless.** Deep queues and `queue_policy: backpressure`, so
  a slow consumer stalls its producer instead of dropping a message. A
  recording made over a lossy edge is a record of what survived, not of what
  was published — and the byte-for-byte claim becomes untestable.
- **The recorder names entries by their *source*.** `Node::input_source`
  answers "which producer port did this actually come from", so an entry reads
  `sensor/frames` whatever the recorder happened to call its own input. The
  same edge recorded by two different recorders yields the same entries.
- **`replay.yml` is a graph, not a template.** Run it as written and the live
  sensor feeds the probe. `astrs replay --into` replaces `sensor` *in place* —
  same id, same `outputs:` — so `frames: sensor/frames` still resolves and no
  other node is touched.

## Two divergences from the manifest shorthand, on purpose

**The recorder is an example node, not `astrs-record-node`.** The production
path is one line of sugar — `record: [sensor/frames, detector/detections]` —
which lowers to the shipped `astrs-record-node` binary. That binary is looked
up on `PATH` (a manifest naming a build directory would not survive being
deployed) and takes its destination from the sugar rather than from the
environment; neither fits an example that must run straight out of a `cargo
build` without writing into the checkout. `arec-writer` does the same job over
the same public API — `astrs-recording` for the container, `Node::input_source`
for the entry names — and the file it writes is an ordinary `.arec`. Nothing
downstream can tell: `astrs bag info`, `astrs replay --into` and
`astrs-replay-node` all read it.

**The `frames` edge is untyped.** Every other example in this estate declares a
type URN on both ends of every edge, and that is the right default.
Here the claim under test is about bytes, so a typed edge would leave room to
argue that a mismatch was an encoding detail rather than a lost message. The
payloads are self-describing instead: the first eight bytes of each one are its
frame index, so a gap or a reordering names itself in the artefact.

## Files

| File | What it is |
|---|---|
| `dataflow.yml` | the live graph; writes `$RECORD_REPLAY_SESSION` |
| `replay.yml` | the replay target; `astrs replay --into` rewrites `sensor` here |
| `src/bin/arec_sensor.rs` | the source, and the node replay takes over |
| `src/bin/arec_detector.rs` | the stage the replay graph swaps out |
| `src/bin/arec_writer.rs` | the recorder |
| `src/bin/arec_probe.rs` | the assertion sink |

| Environment variable | Default |
|---|---|
| `RECORD_REPLAY_SESSION` | `$TMPDIR/astrs-record-replay-session.arec` |
| `RECORD_REPLAY_SEQUENCE` | `$TMPDIR/astrs-record-replay-sequence.json` |
| `RECORD_REPLAY_FRAMES` | `12` (set by the manifests) |
