# astrs-telemetry

Tracing setup, an allocation-free metrics registry and a pure-Rust OTLP/HTTP
exporter for AstRS.

Observability without the `opentelemetry`/`tonic` dependency tree:
`init_telemetry` — shared `tracing` subscriber setup for every AstRS
process, `RUST_LOG`-style env-filter directives, human/JSON rendering that
reuses `astrs-log`'s deterministic formatters, per-process attributes;
`MetricRegistry` — an in-process registry of `Counter`/`Gauge`/`Histogram`
handles with an allocation-free record path once a handle is resolved, and
bounded-cardinality label sets that a hostile or buggy label value cannot
drive to unbounded memory; `CpuMemSampler` — per-process CPU%/RSS sampling
for self and child pids, honestly reporting degraded fidelity where the
platform can only offer it (macOS has no `/proc`); W3C `traceparent`
propagation over `astrs_wire::Metadata` so publish → deliver → process
causality survives the wire; hand-rolled OTLP/HTTP+JSON schema types; and
(feature `telemetry-export`, on by default) an exporter shipping both
signals over `oxihttp` to any OTLP/HTTP collector, with a bounded queue and
drop counter absorbing an unreachable collector without unbounded growth.

See the [crate documentation](https://docs.rs/astrs-telemetry) for the
observability design.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
