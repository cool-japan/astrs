# `rust-pipeline`

The canonical typed graph: three OS processes, two typed edges, and a type
URN on every port.

```bash
cargo build -p rust-pipeline
astrs validate examples/rust-pipeline/dataflow.yml   # type-checks the graph
astrs run      examples/rust-pipeline/dataflow.yml
```

```
astrs/timer/hz/20 ──► [camera-sim] ──frames──► [detector-sim] ──detections──┐
                             │                                              │
                             └──────────────frames──────────────────────────┴──► [recorder-sim]
```

`camera-sim` publishes `std/media/v1/Image[pixel=rgb8]`, a **`std` registry
type** whose columnar layout depends on its URN parameter.
`detector-sim` publishes `std/vision/v1/Detections`, **your own struct** with
`#[derive(AstrsMessage)]` — the flagship node-API example, verbatim — and
reads its input straight out of the payload buffer. `recorder-sim` subscribes
to both
edges, decodes both, and writes a JSON tally to `$PIPELINE_SUMMARY` (default:
a file under the system temporary directory) when its inputs close.

Three details are worth copying:

- **The detector's input declares `queue_size: 2` and `queue_policy:
  drop_oldest`**. A perception stage that falls behind loses *old*
  frames instead of growing a queue, and that costs no code — which is why the
  recorder legitimately sees fewer detections than frames on a busy machine.
- **The detector declares `restart_policy: on_failure` with `max_restarts: 5`**
  — worth restarting a few times, not forever.
- **The control lane pre-empts queued data**, so a node that leaves the moment
  it sees `InputClosed` abandons whatever was still queued behind it. Both
  consumers here drain their stream to empty before finishing.

**One divergence from the canonical form, on purpose.** That form writes the
sink as `record: [camera/frames, detector/detections]`, a one-line sugar that
expands to `astrs-record-node` and a `.arec` file. This example keeps its
*shape* with an ordinary `path:` node so the graph is three real processes
today and the tally is a file the conformance suite can read.
