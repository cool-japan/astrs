# astrs-coordinator

The AstRS cluster coordinator: daemon registry, dataflow FSM, build
orchestration and fan-out.

Owns cluster-wide truth and hands it out over the single wire protocol —
one coordinator per cluster: the daemon registry (registration,
heartbeats, capability negotiation and eviction), the dataflow finite state
machine from `build` through `start`, `stop` and `destroy`, aggregating
per-node exit causes into a typed `DataflowResult`, build orchestration and
artifact serving to daemons, log and topic fan-out to CLI subscribers over
the standard framing, the parameter store API over `astrs-store`, and
sequence-numbered `StateCatchUp` for daemons that reconnect after a
partition.

## High availability (the `ha` feature)

One coordinator is one point of failure for the whole control plane. With
`--features ha`, an odd-sized set of coordinators replicates the durable
registry through Raft (`astrs-raft`):

```sh
# The same peer list on every machine; only --ha-node-id differs.
astrs coordinator --ha-node-id 1 \
    --ha-peer 1=10.0.0.1:7601 \
    --ha-peer 2=10.0.0.2:7601 \
    --ha-peer 3=10.0.0.3:7601
```

- **Mutating verbs** are accepted only by the leader, and only once a
  majority has the entry. Parameters are proposed *before* anything is
  written locally; the orchestration verbs run on the leader and have the
  registry writes they produced captured and replicated.
- **Reads** are served locally under a leader lease — no consensus round
  trip — gated on the leader having committed an entry of its own term.
- **A follower** answers a mutating request with the ordinary structured
  error (`Unavailable`) carrying `leader: <address>` in its context. No wire
  enum gained a variant for this.

Off by default: a single-machine deployment carries no consensus protocol,
no second listener and no write-ahead log it never uses.

See the workspace [README](https://github.com/cool-japan/astrs#architecture)
for the coordinator's place in the process topology, and the [crate
documentation](https://docs.rs/astrs-coordinator) for reconnect/fault
tolerance and coordinator HA.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
