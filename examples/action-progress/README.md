# `action-progress`

A long-running action with progress feedback, over the
`goal_id`/`goal_status` FSM:

```bash
cargo build -p action-progress
astrs run examples/action-progress/dataflow.yml
cat "${TMPDIR:-/tmp}/astrs-action-progress-report.json"
```

```
  [client] ──goal────► [server]
     ▲                     │
     └──────status─────────┘
```

## What this proves

`client` submits three goals — `[5, 8, -3]` — one at a time. `server`
executes each in four steps: three `Executing` status updates carrying
increasing fractional progress (`0.25`, `0.5`, `0.75`), riding the *same*
`status` edge and the *same* `goal_id` as the terminal update that follows —
there is no separate feedback topic, because the pattern does not need one. The
third target is negative on purpose, so `server` reports `Aborted` instead
of `Succeeded`: a demo where every goal succeeds cannot tell a reader
whether `Aborted` was ever actually wired up.

`client` walks every update through a
[`GoalTracker`](https://docs.rs/astrs-node-api), which enforces the FSM
itself — a status out of order, or a second terminal status for the same
goal, is refused rather than accepted. Each run is recorded as a `GoalRun`
(the feedback sequence and the terminal status observed) and is only
`is_correct()` if the terminal status matches the target's sign **and** the
feedback was non-empty and strictly increasing. The full session is written
to `$ACTION_PROGRESS_REPORT` as an `ActionRunReport`, and the client exits
non-zero unless every goal was correct.

## Why the client runs its goals one at a time

`GoalTracker` can track any number of goals concurrently, but this example's
claim — feedback arrives in increasing order and ends in the right terminal
status — does not need concurrent goals to be checkable, and a sequential
run reads start to finish without interleaved bookkeeping.

## Files

| File | What it is |
|---|---|
| `dataflow.yml` | the two-node graph, `pattern: action-client` / `pattern: action-server` |
| `src/bin/progress_client.rs` | submits goals, tracks the FSM, records feedback |
| `src/bin/progress_server.rs` | executes each goal in four steps |
| `src/lib.rs` | the goal/feedback schedule and `ActionRunReport`, all unit-tested |

| Environment variable | Default |
|---|---|
| `ACTION_PROGRESS_REPORT` | `$TMPDIR/astrs-action-progress-report.json` |
