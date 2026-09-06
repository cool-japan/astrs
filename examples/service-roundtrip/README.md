# `service-roundtrip`

Request/response over two ordinary edges.

```bash
cargo build -p service-roundtrip
astrs run examples/service-roundtrip/dataflow.yml
```

```
[client] ──request(std/core/v1/Int64)──► [server]
    ▲                                        │
    └────────response(std/core/v1/Int64)──────┘
```

There is no RPC subsystem. A request is a message whose metadata carries a
`request_id`; a response is a message that copies that id back.
`Node::service_request` mints the id and hands it to the caller,
`ServiceRequest::from_event` confirms an incoming message really is a call, and
`Node::service_response` correlates the answer from the request's own metadata
so a server cannot reply with the wrong id. The client keeps several calls in
flight and matches each answer by id — no sequence discipline of its own.

`pattern: service-client` / `service-server` in the manifest declares the
roles. `astrs validate` uses them to check the pair is wired in both
directions, and the scheduler uses them to grant correlated messages
queue-eviction immunity so an answer is never dropped to make room for
a camera frame. The `client → server → client` cycle is legal precisely
because the pattern covers it: every request has exactly one response,
so the loop terminates.

The client checks every answer (the server squares its input), then writes a
JSON result to `$SERVICE_RESULT` and exits non-zero if any call went
unanswered, was answered wrongly, or arrived uncorrelated.
