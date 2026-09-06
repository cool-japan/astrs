# astrs-runtime

The AstRS operator host: shared event loop, per-operator threads and panic
isolation.

A node-shaped process that hosts several in-process operators for
low-latency chains, so a crop → NMS chain costs a function call rather
than an IPC hop. One shared demux loop feeds every hosted operator; each
runs on its own thread (spawned inside one `std::thread::scope`, so
`RuntimeHost::run` can borrow its `Routing`, `OperatorInbox`es and
`OutputSink` without an `Arc`) with panic isolation — a misbehaving stage
fails its operator, not the host — and in-place restart on the failing
operator's own thread, so a sibling's delivery is never stalled by another
operator's backoff sleep. `OperatorInbox` is built directly on
`astrs-scheduler`'s `InputQueue`/`EventMux` rather than a plain channel,
because a plain channel has no notion of per-input queue policy or
eviction immunity for a `Stop`/`Reload`. Shutdown follows `Stop` → drain
channels → `on_stop` each operator → join threads, in that order.

Two non-default features add operator sources beyond the compiled-in
registry, both driven through the same restart loop so the host never
needs to tell them apart from a compiled-in operator: `dylib-operators`
resolves a manifest `operators[].dylib:` path relative to the dataflow
file, opens it as a platform shared library via `libloading`
(`dlopen`/`LoadLibrary`, no C compiled, no build script) and checks the
ABI version and operator name it advertises before bridging it in;
`wasm-operators` does the same for an `operators[].wasm:` module, compiled
once and instantiated fresh on every restart on `wasmi` — a pure-Rust
interpreter with no JIT — talking to the host over a private guest ABI
(`astrs-op-alloc`/`-init`/`-event` exports, one `astrs.output-send`
import). Both are off by default: a runtime that only ever hosts
compiled-in operators pays for neither loader.

See the workspace [README](https://github.com/cool-japan/astrs#writing-a-node)
for the operator API this crate hosts.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
