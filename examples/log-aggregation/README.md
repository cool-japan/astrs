# `log-aggregation`

Multi-node structured logs, collected through the daemon rather than through
any ordinary edge — the `astrs/logs` virtual source, and AstRS's
observability story:

```bash
cargo build -p log-aggregation
astrs run examples/log-aggregation/dataflow.yml
cat "${TMPDIR:-/tmp}/astrs-log-aggregation-report.json"
```

```
  [sensor-a] ──┐
               ├──► astrs/logs ──► [aggregator]
  [sensor-b] ──┘
```

## What this proves

`sensor-a` and `sensor-b` are the *same* binary (`log-worker`), each given
its own identity and tick cadence through `env:` — the same reuse
`restart-policies` makes of its own single worker binary. Each emits six
marker-tagged log lines, cycling through every severity from `trace` to
`error`, using the ordinary `Node::log`/`log_info`/… API — nothing about
this graph is special-cased for logging.

`aggregator` reads them all back through the *virtual* `astrs/logs` input
— there is no `node/output` edge to either worker at all, because a
log is not a message either node's graph wiring produces on purpose.
Everything the daemon fans out arrives there: the workers' marker lines,
yes, but also the daemon's own records and any node's captured
stdout/stderr. `LogTally::accept` tells a marker apart from the rest and
tallies which `(worker, sequence number)` pairs actually showed up; anything
that is not a marker is simply not counted, neither toward nor against
completeness. The run is written to `$LOG_AGGREGATION_REPORT`, and the
aggregator exits non-zero unless every one of both workers' twelve markers
arrived.

## Two `LogRecord` types, and why this crate only trusts one

`Node::log`/`log_with_fields` build an `astrs_wire::LogRecord` — the frame a
node sends *to* the daemon. What a subscriber reads back *off*
`astrs/logs` is a different, same-named type: `astrs_log::LogRecord`,
documented on that crate as "the JSON payload format carried on
`astrs/logs/*` virtual inputs". `aggregator` decodes every payload as
`astrs_log::LogRecord` — never the wire type — and this example's own claim
rests only on the two fields both types unambiguously agree on end to end:
`node` and `message`. A structured field attached with `log_with_fields`
would be a more elegant way to carry a marker, but this example deliberately
does not lean on it, so its claim does not quietly depend on exactly how
that field survives the wire-to-virtual-source conversion.

## Files

| File | What it is |
|---|---|
| `dataflow.yml` | the three-node graph: two workers, one aggregator on `astrs/logs` |
| `src/bin/log_worker.rs` | emits marker-tagged records at every severity |
| `src/bin/log_aggregator.rs` | decodes every `astrs/logs` payload and tallies the markers |
| `src/lib.rs` | the marker codec and `LogTally`, all unit-tested |

| Environment variable | Default |
|---|---|
| `LOG_WORKER_ID` | `worker` (set per instance by the manifest) |
| `LOG_WORKER_RECORDS` | `6` (set by the manifest) |
| `LOG_AGGREGATION_REPORT` | `$TMPDIR/astrs-log-aggregation-report.json` |
