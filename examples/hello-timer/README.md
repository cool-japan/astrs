# `hello-timer`

The smallest AstRS dataflow that does something: one node, one input, no
producers.

```bash
cargo build -p hello-timer
astrs run examples/hello-timer/dataflow.yml
```

The input is a **virtual source** — `astrs/timer/millis/200`, the daemon's own
timer wheel — so the node receives an ordinary `Event::Input`
five times a second and there is no timer API to learn. It logs one line per
tick through `Node::log_info`, which stamps the record with the node's hybrid
logical clock and hands it to the daemon; `astrs run` streams it to the
terminal, `astrs logs` can tail it, and an in-graph `astrs/logs/*` subscriber
can consume it as data. After ten ticks the node returns, and because the
manifest sets `exit_when_nodes_finish: true` the whole run ends with exit
code 0. `HELLO_TIMER_TICKS` in the manifest's `env:` block changes the count
without a recompile.
