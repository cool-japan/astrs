# astrs-cli

The astrs command line: run, build, monitor, migrate and bridge robotic
dataflows.

The `astrs` binary's library half — everything `main.rs` needs beyond
parsing `argv` and choosing a process exit code. Every verb is implemented
as a library-testable function under `command`, taking a `&mut dyn
std::io::Write` sink and a typed args struct, returning a typed report;
`dispatch` maps a parsed CLI invocation onto the right `command::*::run`
call and flattens whatever it returns into a process exit code. Covers the
full verb set — lifecycle (`run`/`up`/`down`/`build`/`start`/`stop`/
`restart`/`destroy`/`clean`), monitoring (`list`/`logs`/`top`/`topic`/
`status`/`trace`), graph ops (`validate [--prove]`/`expand`/`graph`/`node`/
`param`), data (`record`/`replay`/`bag`), ROS 2 diagnostics (`ros2 doctor`/
`ros2 topics`), cluster admin (`hub init`/`update`/`search`/`info`, `token
mint`), and dev tooling (`new`/`migrate`/`doctor`/`completion`/`schema`) —
every verb reaches a real implementation. A mutating verb
(`start`, `stop`, `destroy`, `param set`, …) authenticates against the
cluster token; a read verb falls back to the all-zero token so a
token-less development coordinator stays inspectable.

## Usage

```sh
cargo install astrs-cli --version 0.1.0   # installs the `astrs` binary
astrs --help
astrs new node hello && cd hello && cargo build --release && astrs run dataflow.yml
```

See the workspace [README](https://github.com/cool-japan/astrs#cli-reference)
for the complete verb table and a verified quickstart.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
