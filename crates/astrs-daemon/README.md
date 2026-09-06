# astrs-daemon

The per-machine AstRS daemon: node supervision, SHM broker, route bridging
and health.

The control-plane actor and reliability path for every node on its host.
One daemon owns every node process on its machine: spawns them with a
scrubbed environment, supervises them under their restart policies, routes
messages between them until the shared-memory plane takes over, holds their
extension table, and stops them — really stops them, `SIGTERM` then
`SIGKILL` on a finish ladder — when the dataflow ends. Pre-split by design
into `spawn/` (process creation, generation-stamped handles), `supervise/`
(restart policies with exponential backoff, exit-cause taxonomy), `local/`
(the daemon-mediated reliable path, `astrs/timer/*` and `astrs/logs/*`),
`extensions/` (crash-safe `(namespace, key) → bytes`), `session/` (the
daemon↔node conversation state machine), `server/` (the merged event loop),
`dataflow/` (planning, building, lifecycle), `peer/` (daemon↔daemon routes
over `astrs-transport`), `shm/` (the same-host zero-copy plane) and
`coordinator/` (the daemon↔coordinator uplink). `run_dataflow_with` hands
the loop a plan and reads the result back in-process — that is `astrs
run`; `Daemon::connect_coordinator` instead dials a coordinator and lets it
drive the same event loop over the wire. Nothing below the seam knows
which one it is running under.

## Example

```rust
# async fn example() -> Result<(), Box<dyn std::error::Error>> {
use astrs_daemon::{RunOptions, run_dataflow_with};
use astrs_manifest::Manifest;

let manifest = Manifest::from_yaml_file("dataflow.yml")?;
let result = run_dataflow_with(&manifest, RunOptions::default()).await?;

for (node, cause) in result.failed_nodes() {
    eprintln!("{node} failed: {cause}");
}
# Ok(()) }
```

See the workspace [README](https://github.com/cool-japan/astrs#architecture)
for the process model, and the [crate
documentation](https://docs.rs/astrs-daemon) for the supervision and
fault-tolerance design.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
