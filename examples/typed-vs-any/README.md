# `typed-vs-any`

A typed columnar port and a raw-bytes port, carrying the identical values,
side by side:

```bash
cargo build -p typed-vs-any
astrs run examples/typed-vs-any/dataflow.yml
cat "${TMPDIR:-/tmp}/astrs-typed-vs-any-report.json"
```

```
  [source] ──typed (std/core/v1/Float64)──► [sink]
     │
     └──────raw (no type URN)─────────────► [sink]
```

## What this proves

`source` publishes the same deterministic value twice on every tick, over
two differently-declared ports:

- **`typed`** — `node.output::<Scalar<f64>>("typed")`. The port's
  declared `output_types: { typed: "std/core/v1/Float64" }` is checked
  against `Scalar<f64>`'s URN at construction, and the wire carries a
  self-describing, one-row Arrow `Float64` column.
- **`raw`** — `RawOutput::send_bytes`, eight hand-encoded little-endian
  bytes. No `output_types`/`input_types` entry names it at all —
  `astrs validate` has nothing to check on this edge, by design.

`sink` decodes both, keeps each port's arrivals in a `PortTally`, and once
both close, compares them index by index. A clean run means: both ports
delivered the same non-empty sequence, in the same order, with no
port-internal mismatch (`value_for(i)` at every index) and no cross-port
disagreement. The result is written to `$TYPED_VS_ANY_REPORT` as a
`ComparisonReport`, and the sink exits non-zero if it is not clean.

## Why the raw port is not just a worse typed port

A typed port is checked once, at construction, against the manifest's
declared URN — after that, the wire format is `astrs-data`'s problem, not
the node author's. A raw port has neither guarantee: the encoding is
whatever the node writes, and a change to it is invisible to
`astrs validate`. Both are legitimate: `typed` is the right default,
and `raw` is what a node reaches for when it is moving bytes it does not
need to interpret — `record-replay`'s `frames` edge is exactly that case.
This example's claim is that *both paths deliver the same value correctly*,
never that one subsumes the other.

## Files

| File | What it is |
|---|---|
| `dataflow.yml` | the two-node graph, one typed edge and one raw edge |
| `src/bin/typed_any_source.rs` | publishes the same value on both ports per tick |
| `src/bin/typed_any_sink.rs` | decodes both, compares, and writes the verdict |
| `src/lib.rs` | the deterministic value sequence, the raw codec and `ComparisonReport`, all unit-tested |

| Environment variable | Default |
|---|---|
| `TYPED_VS_ANY_SAMPLES` | `200` (set by the manifest) |
| `TYPED_VS_ANY_REPORT` | `$TMPDIR/astrs-typed-vs-any-report.json` |
