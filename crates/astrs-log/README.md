# astrs-log

Structured log records, rotation and level-filtered virtual-input fan-out
for AstRS.

The log plumbing behind `astrs logs -f` and the `astrs/logs/*` virtual
inputs: `LogRecord` — the structured log entry and its JSON wire encoding
(the payload format on `astrs/logs/*`), with conversions from a `tracing`
event and from a spawned node's captured stdout/stderr; `LogLevel` — with a
total, direction-flipped conversion to/from `tracing::Level`;
`RotatingWriter` — a size-rotating, retention-limited, mutex-guarded
line-oriented JSON file writer for on-disk daemon logs; `LogFileReader` —
reads a rotated log file back; deterministic human-text and JSON rendering;
`LogMerger`/`merge_logs` — an HLC-ordered k-way merge of multiple
`LogRecord` streams, the core of cross-machine `astrs logs -f`;
`LogFilter` — parses `astrs/logs[/level[/node]]` virtual-input paths into a
level+node predicate; and `EnvFilterLite` — a small `RUST_LOG`-style
directive parser (`"info,astrs_daemon=debug"`) for target-scoped filtering.
`HlcTimestamp` is re-exported from `astrs-time` rather than redefined, so
every `LogRecord` and every caller share exactly one clock type.

See the [crate documentation](https://docs.rs/astrs-log) for where this
crate's output lands, including the daemon's virtual log sources.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
