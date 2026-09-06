# `dynamic-add-remove`

`astrs node add`/`astrs node remove` against a running dataflow:

```bash
cargo build -p dynamic-add-remove
astrs run examples/dynamic-add-remove/dataflow.yml
```

That runs the base graph alone — `anchor`, ticking for a little over a
minute, nobody reading it yet. The interesting part needs a *backgroundable*
cluster to send `node add`/`node remove` requests to, which is what
`astrs up` (not `astrs run`) is for:

```bash
cargo build -p dynamic-add-remove -p astrs-cli
astrs up examples/dynamic-add-remove/dataflow.yml &
astrs list                              # find the running dataflow's name/id
astrs node add   dynamic-add-remove examples/dynamic-add-remove/fragments/worker.yml --start
astrs status     dynamic-add-remove     # `worker` is now running, reading anchor/beats
cat "${TMPDIR:-/tmp}/astrs-dynamic-add-remove-proof.json"
astrs node remove dynamic-add-remove worker
astrs down       dynamic-add-remove
```

```
  [anchor] ──beats──►  (nobody, until `astrs node add`)
```

`dataflow.yml` declares exactly one node, `anchor`, and runs it for a little
over a minute (2000 beats at 50 ms) — long enough that the cluster is
genuinely still up while a reader works through the recipe above.
`astrs up` (not `astrs run`) is what starts it, because `astrs up` is meant
to be backgrounded; this example's whole point needs a live coordinator to
send `node add`/`node remove` requests to.

## What `node add` actually does

`fragments/worker.yml` is a **single-node manifest fragment** — the
manifest's own per-node schema, with no `nodes:` wrapper — not a member of
`dataflow.yml`'s own graph. `astrs node add` reads it, expands it into a
spawn specification via `astrs_coordinator::expand_node_fragment` (the same
function `bins/astrs-cli`'s own `node add` command calls), and asks the
coordinator to spawn it wired to whatever it declares — here,
`inputs: { beats: anchor/beats }`, a sibling that already exists in the
running graph. `worker` counts the beats it actually observes and writes a
`WorkerProof` to `$DYNAMIC_WORKER_PROOF`, so `cat`-ing that file after
`node add --start` is how a reader checks the new node was not merely
spawned but really wired up. `astrs node remove dynamic-add-remove worker`
then removes it; `anchor` is unaffected either way.

## Why a fragment's `path:` is not resolved like `dataflow.yml`'s

`astrs run`/`astrs up` rewrite a manifest's own node `path:` entries
relative to *that manifest's own directory* before anything is spawned —
which is why every other example's `dataflow.yml` writes
`../../target/debug/…`. `astrs node add` has no such manifest context: it
reads the fragment file directly, and `expand_node_fragment` carries
`path:` through unchanged, with no rewriting at all. So `fragments/
worker.yml`'s `path:` is written relative to the **workspace root** —
`./target/debug/dynamic-worker` — because that is where the recipe above
runs every command from, `astrs up`/`astrs node add` included, and that is
what a relative `path:` resolves against once nothing has rewritten it.

## Why this crate's own test does not drive a real coordinator

Every other example in this estate proves its central claim with a
`-p`-scoped unit test needing nothing but its own code. Proving that
`node add` really attaches a live process to a live cluster needs a real
coordinator and daemon — squarely `tests/conformance`'s job, not this
package's. What this crate's own test checks instead, cheaply and for real:
that `fragments/worker.yml` is exactly the well-formed fragment `node add`
expects, by reproducing `expand_node_fragment`'s own logic (parse the bare
`Node`, wrap it in a synthetic one-node manifest, build a graph from it) —
see `dynamic_add_remove::tests::the_fragment_expands_the_way_expand_node_fragment_would`.

## Files

| File | What it is |
|---|---|
| `dataflow.yml` | the base graph: `anchor` alone |
| `fragments/worker.yml` | the single-node fragment `astrs node add` reads |
| `src/bin/dynamic_anchor.rs` | publishes the incrementing counter |
| `src/bin/dynamic_worker.rs` | the node added dynamically; proves it observed live beats |
| `src/lib.rs` | the beat codec and `WorkerProof`, all unit-tested |

| Environment variable | Default |
|---|---|
| `DYNAMIC_ANCHOR_BEATS` | `2000` (set by the manifest) |
| `DYNAMIC_WORKER_BUDGET` | `20` (set by the fragment) |
| `DYNAMIC_WORKER_PROOF` | `$TMPDIR/astrs-dynamic-add-remove-proof.json` |
