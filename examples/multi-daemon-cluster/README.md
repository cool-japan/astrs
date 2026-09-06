# `multi-daemon-cluster`

One dataflow, two machines, and the crossing asserted from *inside* the graph
— the demo behind milestone M2.

```bash
cargo build -p multi-daemon-cluster
astrs run examples/multi-daemon-cluster/dataflow.yml   # one machine, placement ignored
```

```
machine robot-a                          machine robot-b
┌──────────────────────┐                 ┌──────────────────────┐
│ [sensor] ──readings──┼════ peer TCP ══►│ [planner]            │
│ [checker] ◄──────────┼◄═══ peer TCP ═══┼── commands           │
└──────────────────────┘                 └──────────────────────┘
         daemon A ──────► coordinator :7407 ◄────── daemon B
```

Both edges cross the machine boundary, in opposite directions: the shortest
graph that exercises a peer route each way.

## The two-machine run, on one localhost

Four terminals — or four background processes, which is what the conformance
suite does in process. Everything below was run to produce the output quoted
at the end.

```bash
cargo build -p astrs-cli -p multi-daemon-cluster
export WD=/tmp/astrs-cluster-demo && mkdir -p "$WD"

# 1. the coordinator (--port 0 picks a free one; --no-daemon: we start our own)
astrs up --json --port 0 --no-daemon --runtime-dir "$WD/rt-c" --working-dir "$WD"
export ADDR=127.0.0.1:<the port it printed>

# 2. two daemons, one per machine name, each with its own runtime directory
astrs daemon --coordinator "$ADDR" --machine robot-a --port 0 --peer-port 0 \
      --runtime-dir "$WD/rt-a" --working-dir "$PWD" \
      --token-file "$WD/.astrs-token" --label zone=front --announce &
astrs daemon --coordinator "$ADDR" --machine robot-b --port 0 --peer-port 0 \
      --runtime-dir "$WD/rt-b" --working-dir "$PWD" \
      --token-file "$WD/.astrs-token" --label zone=back --announce &

# 3. start the graph — note the ABSOLUTE manifest path, see below
astrs start "$PWD/examples/multi-daemon-cluster/dataflow.yml" \
      --coordinator "$ADDR" --working-dir "$WD" --name cluster
astrs logs   cluster --coordinator "$ADDR" --working-dir "$WD"
astrs status cluster --coordinator "$ADDR" --working-dir "$WD"

# 4. tear it down
astrs down --runtime-dir "$WD/rt-c" --working-dir "$WD"
```

**Use an absolute manifest path.** `astrs start` sends the manifest's own
directory as the dataflow's working directory, and each daemon resolves node
`path:` entries against it. A *relative* manifest path therefore arrives as a
relative working directory and is resolved a second time on the daemon's side,
against wherever that daemon was started — which fails with `No such file or
directory` for a binary that is plainly there. On a real cluster the same rule
has teeth for a different reason: every machine must have the binaries at the
path the manifest names.

## How a node proves it crossed a machine boundary

It cannot, on its own — and that is the design working. A consumer sees an
ordinary `Event::Input` whichever plane carried it; `Node::input_plane` only
distinguishes this host's daemon path from its shared-memory ring, never a
remote peer.

So each node stamps **its own placement** into what it publishes. A node knows
where it is running: the daemon that spawned it put the answer in its spawn
spec, and `Node::descriptor().deploy` reads it back. A `Command` therefore
carries both the planner's machine and the machine the reading it answers came
from, and the checker compares three names it did not choose — the sensor's,
the planner's, and its own. Two adjacent names that differ *are* a machine
crossing, witnessed by the application rather than inferred from a log line.

The verdict goes to `$CLUSTER_REPORT` (default: a file under the system
temporary directory) as JSON, which is what
`tests/conformance/tests/m2_multi_daemon.rs` asserts on:

```json
{
  "sensor_machine": "robot-a",
  "planner_machine": "robot-b",
  "checker_machine": "robot-a",
  "checker_labels": { "role": "sensing" },
  "expected": 12,
  "received": 12,
  "in_order": true,
  "inputs_closed": true,
  "problems": []
}
```

```
[checker] verdict: 12/12 commands, robot-a -> robot-b -> robot-a, crossed two machine boundaries
```

## The same manifest on one machine

`astrs run` embeds a single daemon and never consults `deploy:`, so the same
file runs end to end on one host with every placement reported as `unplaced`
and `crossed_twice: false`. That is a legitimate way to try the graph before
setting up a cluster, so the checker still exits zero — only the *cluster*
test demands the crossing, because only it arranged for one.

## `deploy:` — what resolves placement, and what merely travels

`deploy.machine` is what the coordinator resolves against its connected
daemons: exactly one daemon must claim that name, or the start is refused
naming the machine it could not place (never silently run on another one).

`deploy.labels` are carried alongside — declared in the manifest, delivered to
the node in its spawn spec, and asserted above to have survived the trip
(`checker_labels`). Each daemon separately advertises its own `--label` set at
registration, visible in `astrs status`. Label *selectors* are not how
placement is resolved in this build; `machine:` is.

## Files

| File | What it is |
|---|---|
| `dataflow.yml` | the split graph: two `deploy.machine`s, two crossings |
| `src/bin/cluster_sensor.rs` | the source on `robot-a`, stamping its own placement |
| `src/bin/cluster_planner.rs` | the far half on `robot-b`, stamping both |
| `src/bin/cluster_checker.rs` | the assertion node, back on `robot-a` |

| Environment variable | Default |
|---|---|
| `CLUSTER_REPORT` | `$TMPDIR/astrs-multi-daemon-cluster-report.json` |
| `CLUSTER_READINGS` | `12` (set by the manifest) |
