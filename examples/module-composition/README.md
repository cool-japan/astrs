# `module-composition`

Two operator modules, composed into one runtime process:

```bash
cargo build -p module-composition
astrs run examples/module-composition/dataflow.yml
cat "${TMPDIR:-/tmp}/astrs-module-composition-report.json"
```

```
  astrs/timer/millis/20 ──► tick ──► [compose] ──result──► [collector]
                                       │  ScaleOperator ──out──► OffsetOperator │
                                       └── one runtime process, one thread each ┘
```

## What this proves

`compose` hosts two `astrs_operator_api::Operator` implementations in one
process: `ScaleOperator` reads the node's own `tick` input and multiplies a
running counter by `SCALE_FACTOR`; `OffsetOperator` reads `ScaleOperator`'s
output *directly* — a function call inside `astrs-runtime`'s own routing,
never an IPC hop — and adds `OFFSET_CONSTANT`, publishing the sum on
`compose`'s own `result` output. `composed_value(index)` is the pure
function the pair together compute, and `collector` checks every value it
receives against it.

## Why `compose` is a `path:` node, not a manifest `operators:` node

The manifest lists `operators:` as a node source of its own, and
`astrs-manifest` parses and validates it — but the daemon spawns a **fixed
binary literally named `astrs-runtime`** for that source kind, and that
generic binary's own docs record a known gap: the wire handshake between
daemon and runtime host today carries each operator's id and registry name,
but not the per-operator `inputs`/`outputs` wiring
`astrs_manifest::OperatorConfig` carries. A *manifest-declared*
`operators:` node cannot yet compose two operators together for real, and
naming a binary `astrs-runtime` here would in any case collide with
`crates/astrs-runtime`'s own `[[bin]]` of that exact name.

`compose-host` sidesteps both problems the way `astrs-runtime`'s own module
docs describe as "a real deployment": a *different* binary that
`register_operator!`s its own types and builds its own `RuntimeConfig`
directly, in Rust — `module_composition::runtime_config()` is exactly that
config, shared so `compose-host`'s `main` and this crate's own test drive
the identical wiring. The daemon still only ever sees an ordinary node with
one input and one output; what happens *inside* it — two operators, two
threads, no IPC between them — is this example's claim, and it holds
regardless of the manifest-level gap.

## Why the central test drives a real `RuntimeHost`

Every other example in this estate proves its claim with pure-function unit
tests. This one is different on purpose: the claim *is* that two operators
compose correctly inside `astrs-runtime`'s own routing and threading, and a
test that only checked `composed_value`'s arithmetic would prove nothing
about composition at all. `Node::init_testing` exists precisely so a test
can run the real thing — a real `RuntimeHost`, the real `ScaleOperator`/
`OffsetOperator`, a real daemon-shaped session — with nothing standing in
but the socket. `module_composition::tests::
the_composed_pipeline_runs_two_operators_in_one_process` feeds a dozen
synthetic ticks through a `MockDaemon`, waits (no sleeps — `MockDaemon::
wait_for_sends` blocks on the real send count) for a dozen `result`
messages, and checks every one against `composed_value` before stopping the
host and inspecting its `RuntimeReport`.

## Files

| File | What it is |
|---|---|
| `dataflow.yml` | the two-node graph: `compose` (the composed host), `collector` |
| `src/bin/compose_host.rs` | connects and hands off to `RuntimeHost` |
| `src/bin/compose_collector.rs` | verifies every result against `composed_value` |
| `src/lib.rs` | `ScaleOperator`/`OffsetOperator`, the shared `RuntimeConfig`, and the `RuntimeHost`-driving test |

| Environment variable | Default |
|---|---|
| `MODULE_COMPOSITION_TICKS` | `40` (set by the manifest) |
| `MODULE_COMPOSITION_REPORT` | `$TMPDIR/astrs-module-composition-report.json` |
