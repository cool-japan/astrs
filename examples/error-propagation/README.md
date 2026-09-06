# `error-propagation`

AstRS's error path, observed from inside the graph:

> Error propagation: a failing node emits `NodeFailed` to graph peers (visible
> on `astrs/status`), the dataflow FSM aggregates per-node exit causes into a
> `DataflowResult` with **typed causes (not strings)**.

```bash
cargo build -p error-propagation
astrs run examples/error-propagation/dataflow.yml   # exits non-zero, on purpose
```

```
astrs/timer/millis/20 ──► [source] ──readings──► [watcher] ──► $FAULT_REPORT
                              └── exit 23 ──► NodeFailed ──►     ▲
                          astrs/status ────────────────────────────┘
```

The source publishes three readings and then exits `23` **without calling
`close()` on its output** — the abrupt end is the point, because the watcher
must learn what happened from the daemon rather than from a goodbye the source
never composed. The watcher writes down what it received:

```json
{
  "readings": 3,
  "failures": [
    {
      "peer": "source",
      "cause_kind": "exit_code",
      "cause": "exited with code 23",
      "exit_code": 23
    }
  ],
  "closed_inputs": ["readings: producer generation 0 crashed"],
  "finished_cleanly": true
}
```

`cause_kind` is the *discriminant* of `NodeExitCause`, not its rendered text.
That is what makes "typed causes, not strings" checkable: a change that turned
the cause into a message would show up as a missing `cause_kind`, not as a
different-looking sentence.

## The closure tells the same story the failure does

`closed_inputs` says **crashed**, and that is the second thing this example is
for (*truthful producer failure*). The source's output handle is dropped on the
way out — every node written in the ordinary node-API style does that, whether
or not it ever calls `close()` — so the daemon receives a closure *before* it
receives the exit status. Answering that closure immediately would commit it
to `ProducerFinished` and hand the watcher two contradictory facts: an input
whose producer "finished", and a peer that exited 23.

So it does not answer immediately. A closure that arrives as **process
teardown** waits for the reap, and then carries `ProducerCrashed{generation}`
or `ProducerFinished` according to what the exit status actually was. An
explicit `output.close()` from a node that goes on running is a different
statement and is still answered on the spot — a live graph must not be made to
wait for an exit that may be hours away.

## A non-zero exit is the result, not a defect

`astrs run` exits with the dataflow's severity, and this graph's source
fails on purpose. What the example demonstrates is what the *other* node saw,
and that the run's `DataflowResult` names the node that failed and the node
that did not.

## Two things worth copying

- **`astrs/status` never closes.** It is a virtual source the daemon serves
  for the life of the dataflow, so a node that declares it and waits for
  `AllInputsClosed` waits for ever — the graph is over, its producer is gone,
  and one input is still nominally live. A supervisor-shaped node ends on the
  closure of the input it actually processes. This example's watcher does, and
  that is the only reason it terminates.
- **Declaring `astrs/status` is intent, not plumbing.** The daemon delivers
  `NodeFailed` to a node because it is *downstream* of the one that failed, so
  the watcher's arm would fire whether or not the input were declared.
  Declaring it is how the manifest says "this node reacts to its peers" —
  `astrs validate` sees it and `astrs graph` draws it.

## Files

| File | What it is |
|---|---|
| `dataflow.yml` | the two-node graph: one node fails, one node is told |
| `src/bin/fault_source.rs` | the node that fails |
| `src/bin/fault_watcher.rs` | the peer that is told, and writes it down |

| Environment variable | Default |
|---|---|
| `FAULT_REPORT` | `$TMPDIR/astrs-error-propagation-report.json` |
| `FAULT_READINGS` | `3` (set by the manifest) |
