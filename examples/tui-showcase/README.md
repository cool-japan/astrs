# `tui-showcase`

`astrs-tui` rendering driven against a scripted cluster snapshot, verified
with `TestBackend` goldens:

```bash
cargo build -p tui-showcase
astrs run examples/tui-showcase/dataflow.yml
```

```
  astrs/timer/millis/40 ──► tick ──► [camera] ──frames──► [detector]
```

The command above runs a real, ordinary dataflow — `camera`/`detector`, the
same pair the canonical manifest example uses. This
example's actual point lives in its test suite:

```bash
cargo test -p tui-showcase
```

## What this proves

`tui_showcase::tests::ScriptedCluster` implements `astrs_tui::ClusterView`
without a coordinator, a daemon, or a clock: it parses *this directory's own*
committed `dataflow.yml` into a real `astrs_tui::view::GraphInfo` (the exact
`astrs_graph::DataflowGraph::from_manifest` call `astrs-tui`'s own
coordinator source makes) and layers fabricated run-time state on top — a
`camera` node shown running with resource metrics, a `detector` node shown
mid-restart with none yet, three log lines, four timeline events covering
every category from `Spawn` to `Violation`.

The test suite then drives a real `astrs_tui::App` against it: a fresh app
opens on the Dataflows tab and renders the scripted state; scripted key
presses (`'2'`, `'3'`, `'4'`, `Tab`, `'q'`) switch tabs exactly the way a
real terminal session would, and each tab is rendered into an 80x24
`ratatui::backend::TestBackend` buffer and checked for the content that tab
alone can show — the real topology and its `[shm]` plane badge on Graph, the
scripted log lines on Logs, the scripted timeline on Timeline. Nothing here
opens a terminal, a socket, or a file at render time; `ScriptedCluster` is
built once, and every golden renders from the same frozen snapshot.

## Why the fixture lives inside `#[cfg(test)]`

`ScriptedCluster` is meaningful only as something this crate's own tests
render — no real node ever constructs one — and building it needs a few
`.expect()` calls (`dataflow.yml` failing to parse, a hard-coded node id
failing its own grammar) that could only ever fire if the fixture itself
were broken. This workspace's policy is "no `unwrap`/`expect` in non-test
code", not "nowhere, ever" — keeping the whole fixture inside
`#[cfg(test)] mod tests`, under the same `#![allow(clippy::unwrap_used,
clippy::expect_used, clippy::panic)]` header every other test module in
this estate already uses, is what makes that distinction hold here without
reaching for a per-function lint override this codebase does not use
anywhere else.

## Files

| File | What it is |
|---|---|
| `dataflow.yml` | the real, runnable graph — and the topology `ScriptedCluster` parses |
| `src/bin/showcase_camera.rs` | publishes one frame per tick |
| `src/bin/showcase_detector.rs` | turns each frame into a trivial detection |
| `src/lib.rs` | the shared frame codec, plus `ScriptedCluster` and every `TestBackend` golden, inside `#[cfg(test)]` |

| Environment variable | Default |
|---|---|
| `TUI_SHOWCASE_FRAMES` | `20` (set by the manifest) |
