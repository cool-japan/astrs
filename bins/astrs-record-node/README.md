# astrs-record-node

The AstRS recorder node: writes dataflow traffic into `.arec` sessions.

A normal graph node — reachable through `record:` manifest sugar or `astrs
record start` — that subscribes to the selected outputs and streams them
into an `.arec` session through `astrs-recording`: HLC-ordered,
zstd-framed and seekable. Convention-agnostic by design: it is spawned two
different ways, and each names this node's inputs differently (`record:`
sugar lowers to `_record_0`, `_record_1`, …; `astrs record start`'s dynamic
path names them `in0`, `in1`, …). The node never looks at its own input
names — for every received event it asks "which producer port did this
actually come from" (a property of the wiring, not of the message) and
uses that answer as the entry's `node`/`output` fields, so the same binary
serves both spawn paths without caring which one it is.

## Usage

```text
astrs-record-node <output.arec> [--rotate-bytes N] [--rotate-seconds N]
```

The daemon supplies `<output.arec>` as this process's first argument;
`--rotate-*` are reserved for a future manifest field or direct invocation
that wants bounded file sizes.

See [`astrs-recording`](https://docs.rs/astrs-recording) for the recording
design this node implements.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
