# `restart-policies`

Supervision, made countable:

> Restart policies per node: supervised respawn with exponential backoff
> (`restart_delay`×2^n capped by `max_restart_delay`), budget `max_restarts`
> within `restart_window`. New incarnation ⇒ new generation.

```bash
cargo build -p restart-policies
astrs run examples/restart-policies/dataflow.yml           # recovers,  exit 0
astrs run examples/restart-policies/budget-exhausted.yml   # gives up,  exit 1
```

Two manifests, one binary. The difference between recovering and running out
of budget is a `max_restarts:` line and an environment variable, not different
code — which is what makes the pair a test of the *supervisor*.

| Manifest | `max_restarts` | The worker | Outcome |
|---|---:|---|---|
| `dataflow.yml` | 5 | fails twice, then works | the run finishes clean, 3 starts |
| `budget-exhausted.yml` | 2 | fails every time | the run fails, exactly 3 starts |

```
start 1  restart_count=0  fail ─┐
start 2  restart_count=1  fail ─┼─ backoff: restart_delay × 2ⁿ (capped)
start 3  restart_count=2  work ─┘
```

## How the count is made durable

Every incarnation appends one JSON line to `$RESTART_LOG` **before** it does
anything else, so a start is recorded even by a process that is about to exit
non-zero. Each line carries the node API's own view of the same fact —
`Node::restart_count()` and `Node::generation()` — so the file and the API
agree, or the conformance suite says which one is wrong:

```json
{"restart_count":0,"generation":0,"will_fail":true}
{"restart_count":1,"generation":1,"will_fail":true}
{"restart_count":2,"generation":2,"will_fail":false}
```

The surviving incarnation also writes `$RESTART_SUMMARY`, so "which start
succeeded" is an artefact rather than a log line to grep.

## What is deliberately not asserted

**Time.** Backoff is `restart_delay × 2ⁿ` capped by `max_restart_delay`, and a
test that measured it would be measuring the machine it ran on. What the
conformance suite checks is the *count* (`1 + max_restarts` starts, exactly),
the generations (distinct, increasing, one per start) and the typed exit cause.
The delays in both manifests are small on purpose so that an example about
counting does not take seconds to demonstrate counting.

## Why the worker asks the daemon how many times it has restarted

A node that counted its own restarts would be counting a file. The daemon
tells each incarnation, through the node API's introspection surface, and
using *that* is what makes this a test of the supervisor rather than of the
example.

## Files

| File | What it is |
|---|---|
| `dataflow.yml` | the flaky node that recovers inside its budget |
| `budget-exhausted.yml` | the same node with a budget it cannot meet |
| `src/bin/restart_worker.rs` | the one binary both manifests run |

| Environment variable | Default |
|---|---|
| `RESTART_LOG` | `$TMPDIR/astrs-restart-policies-log.txt` |
| `RESTART_SUMMARY` | `$TMPDIR/astrs-restart-policies-summary.json` |
| `RESTART_FAILURES` | `2` (set by the manifests) |
