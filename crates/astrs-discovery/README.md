# astrs-discovery

Static peer lists and UDP multicast beacons for AstRS daemon and coordinator
rendezvous.

How a daemon finds its coordinator and its sibling daemons without a
routing middleware in the middle: static peer lists from configuration and
environment (`PeerBook`), a UDP multicast beacon (`Beacon`) with periodic
jittered announcements, HMAC-authenticated frames, liveness expiry and
dedup by daemon identity, and the rendezvous function
(`rendezvous::discover_coordinator`) that feeds confirmed peers into
`astrs-transport` for connection establishment. Multicast is routinely
unavailable in sandboxes, containers and CI runners, so every layer here is
built to degrade rather than fail when it is missing: a socket bind reports
a degraded status instead of erroring on a failed group join, the beacon
sender keeps sending to its unicast fallback list, and a rendezvous
collection window that sees no beacons at all simply returns an empty list.
A deployment that cannot use multicast at all still works end to end
through `PeerBook` and unicast fallback addresses alone.

See the workspace [README](https://github.com/cool-japan/astrs#architecture)
for where rendezvous sits in the coordinator/daemon process topology.

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
