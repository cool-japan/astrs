# astrs-replay-node

The AstRS replay node: re-injects recorded `.arec` outputs into a
dataflow.

Reads an `.arec` session and re-injects the recorded outputs as sources,
replacing any subset of the original nodes — so a new planner can be
tested against last week's sensor data byte for byte, at a configurable
speed or in lockstep. `astrs replay <file> --into <manifest>` (the CLI verb
this binary serves) rewrites the target manifest **in place**: a replaced
node keeps its own `id` and declared `outputs`; only its `path:` becomes
`astrs-replay-node` and its `args:` gain `--only <node>/<output>` per
output. Every sibling's `inputs:` therefore still reads `camera/frames` —
nothing about the graph's wiring changes, only what actually produces it.
Three timing modes: `AsFastAsPossible` (no pacing), `RealTime` (sleeps the
recorded HLC delta, scaled by `--speed`), and `FixedRate` (a constant `1 /
--rate` seconds between entries, ignoring recorded deltas). Every
republished message carries a **fresh** HLC timestamp from this node's own
clock — replay is a new live event, not a literal replica of history —
while every other metadata field (`seq`, correlation keys, …) passes
through unchanged, which is what keeps the payload bytes exactly
reproducible while the timestamps stay monotone on their own timeline.

See [`astrs-recording`](https://docs.rs/astrs-recording) for the replay design
this node implements.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
