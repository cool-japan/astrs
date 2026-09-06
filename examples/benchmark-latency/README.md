# `benchmark-latency`

The [performance gates'](https://github.com/cool-japan/astrs#performance-gates)
"same-host small message RTT" row, made into a runnable, self-timing graph:

```bash
cargo build -p benchmark-latency
astrs run examples/benchmark-latency/dataflow.yml
cat "${TMPDIR:-/tmp}/astrs-benchmark-latency-report.json"
```

```
  [prober] ──ping──► [reflector]
     ▲                    │
     └───────pong─────────┘
```

## What this proves

`prober` holds exactly **one** round trip in flight: it sends `ping`, waits
for the matching `pong`, times the gap with its own clock, and only then
sends the next `ping`. That closed-loop discipline is what makes the number
a round-trip latency rather than a queueing delay — an open-loop sender
(fire on a fixed timer regardless of whether the last reply arrived) would
let a slow reflector's backlog leak into every sample after the first, and
the report would measure the queue, not the wire.

`reflector` does the minimum possible: read `ping`, write the same bytes to
`pong`, immediately. Whatever time separates a `ping` send from its `pong`
arrival is (daemon relay out) + (reflector's own scheduling) + (daemon relay
back) — the local, same-host, daemon-mediated small-message path the
performance gates call out.

When the budget (`BENCHMARK_LATENCY_SAMPLES`, default 300) is spent, `prober`
writes a `LatencyReport` — `samples`, `mismatched`, `min_us`, `p50_us`,
`p90_us`, `p99_us`, `max_us`, `mean_us` — to
`$BENCHMARK_LATENCY_REPORT` (default: a file under the system temporary
directory) and exits non-zero if the report fails its own internal sanity
check.

## Why a clock reading never crosses a process boundary

`std::time::Instant` is never serialized onto the wire and compared against
a reading taken in `reflector`. The standard library gives no portable
guarantee that two processes' monotonic clocks share an epoch, so a
`reflector`-side timestamp subtracted from a `prober`-side one would measure
clock skew as often as it measured latency. `prober` instead times *itself*:
one `Instant::now()` immediately before `ping` is sent, one immediately after
the matching `pong` arrives, both read by the same clock in the same
process. The 8-byte sequence number riding in the payload is only there so
`prober` can recognise which `pong` answers which `ping` (and notice a
mismatch, which it counts rather than mistiming) — it plays no part in the
timing itself.

## The percentile definition, pinned

`percentile_us` is the classic **nearest-rank** percentile: the
`ceil(p * n)`-th smallest sample, no interpolation. `p` is expressed as
**permille** (parts per 1000 — `500` for p50, `990` for p99) so the rank is
exact integer arithmetic. An `f64` fraction (`0.99 * n`) can land a hair
either side of an integer boundary depending on `n`, which would make `ceil`
silently pick a different rank for the "same" nominal percentile on two
different sample counts; permille has no such edge, by construction.

## Loose sanity bounds, on purpose

The bound this example asserts (`SANITY_P99_CEILING_US`, 250 ms) is three
orders of magnitude above the aspirational same-host RTT target (25 µs)
— not the target itself. This example's own `-p`-scoped test runs under
parallel `cargo nextest`, on whatever machine happens to be building the
workspace: a CI runner, a laptop under load, a container with a noisy
neighbour. A bound tight enough to catch a real regression is also tight
enough to fail there for reasons that have nothing to do with AstRS.
`astrs-bench`'s criterion suite is where the real target belongs; this
example only proves the measurement pipeline itself — timing, correlation,
percentiles — is wired correctly, with a ceiling loose enough that only a
genuinely broken wire (a stuck reflector, a wedged queue) would trip it.

## Why neither port carries a type URN

Every other example in this estate declares a type URN on both ends of every
edge, and that is the right default. Here the payload is a bare
8-byte sequence number standing in for "a small message" — the claim under
test is about the wire's timing, not about encoding, so a typed port would
add nothing but ceremony (the same reasoning `record-replay`'s `frames` edge
documents for its own untyped port).

## Files

| File | What it is |
|---|---|
| `dataflow.yml` | the two-node graph |
| `src/bin/latency_prober.rs` | the closed-loop timer and percentile writer |
| `src/bin/latency_reflector.rs` | the echo |
| `src/lib.rs` | the payload codec, the percentile function and `LatencyReport`, all unit-tested |

| Environment variable | Default |
|---|---|
| `BENCHMARK_LATENCY_SAMPLES` | `300` (set by the manifest) |
| `BENCHMARK_LATENCY_REPORT` | `$TMPDIR/astrs-benchmark-latency-report.json` |
