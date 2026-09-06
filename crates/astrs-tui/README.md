# astrs-tui

The ratatui live monitor for AstRS: graph view, node stats, HLC timeline
and log tail.

What `astrs top` opens: a live ratatui view of a running cluster, or of an
`.arec` recording played back with no cluster at all (`astrs top
--replay`). Four tabs: **Dataflows** (the dataflow list and the selected
dataflow's per-node status/restarts/CPU/RSS/queue-depth table), **Graph**
(a compact layered ASCII rendering of the running topology with plane
badges), **Logs** (a level/node-filterable live tail, formatted identically
to `astrs logs`), and **Timeline** (recent lifecycle events, HLC-ordered).
Every tab renders from a `&ClusterSnapshot`, never from a live connection
directly — `ClusterView` is the trait that produces one, with two
implementations (a polled live coordinator, or an `.arec` replay) — which
is what makes the whole crate testable with `ratatui::backend::TestBackend`
and no terminal, no coordinator, and no file: every renderer, and the
input-handling state machine, is a pure function of a snapshot and the UI
state.

See the workspace [README](https://github.com/cool-japan/astrs#cli-reference)
(`astrs top`) and the [crate documentation](https://docs.rs/astrs-tui) for the
design this crate implements.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
