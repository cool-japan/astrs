# astrs-store

Coordinator persistence for AstRS parameters, dataflow state and the build
cache index.

A thin, schema-versioned persistence layer over `oxistore-core`/
`oxistore-kv-redb`: the parameter store backing `astrs param
get/set/list/delete`, dataflow lifecycle state (so a restarted coordinator
resumes rather than forgetting the cluster), the daemon registry, and the
build cache index for artifact reuse across `astrs build` runs. A
schema-version marker is checked on every open, with an explicit
`recreate_store` escape hatch. Every mutating call also appends to a
seq-numbered mutation log, so a reconnecting daemon can be resynchronised
with `StateCatchUp` instead of re-sent the coordinator's entire state.
`CoordinatorStore` is generic over any `oxistore_core::KvStore`, used two
ways — a real file for a real deployment, and an ephemeral in-memory
database for tests and `astrs run`'s embedded single-process coordinator —
through the identical trait surface either way. Buckets store JSON (the
human-readable boundary); the mutation log stores `oxicode` (compact,
high-volume, never read by a person directly).

See the workspace [README](https://github.com/cool-japan/astrs#architecture)
for this crate's role in the coordinator.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
