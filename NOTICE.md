# NOTICE

AstRS
Copyright 2026 COOLJAPAN OU (Team Kitasan)

This product is licensed under the Apache License, Version 2.0 (the
"License"); you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS, WITHOUT
WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied. See the
License for the specific language governing permissions and limitations under
the License.

## Reference material

The following COOLJAPAN Ecosystem projects — same author organization,
unpublished at the time of writing — were consulted as reference
implementations while building the AstRS ROS 2 pillar. AstRS does **not**
depend on them; where protocol tables, test vectors or state-machine structure
were adapted rather than re-derived, the adapted files carry an in-file
attribution comment naming the source project.

| Project | Consulted for |
|---|---|
| `oxicar-ros2` (OxiCar) | RTPS reader/writer state machines, SPDP/SEDP details, CDR corner cases, tf2 semantics, protocol test vectors |
| `oxictl` (`protocol/dds`) | `no_std` RTPS 2.3 submessage grammar, scheduler and deadline-monitor patterns |
| `oxi3d-ros2` (Oxi3D) | `PointCloud2` layouts, rosbag handling |

Specifically, in `astrs-rtps`'s discovery half the following **protocol
tables** were cross-checked against `oxictl`'s `protocol/dds` rather than
derived from the OMG documents alone, and are adapted in the sense described
above:

- the `PID_*` set and per-parameter value layouts of the SPDP participant
  sample (`src/discovery/participant_data.rs`) and the two SEDP endpoint
  samples (`src/discovery/endpoint_data.rs`);
- the CDR wire layouts and enumerator values of the DDS QoS policies
  (`src/discovery/qos.rs`) — in particular `RELIABILITY`'s one-based wire
  numbering, which differs from the DDS API's;
- the `BuiltinEndpointSet` bit assignments (`src/discovery/builtin.rs`).

Everything else in that crate — the behaviour half's writer, reader, proxy,
history-cache, fragmentation, liveliness and participant modules — is an
independent implementation against OMG DDSI-RTPS 2.3, re-architected on tokio
rather than adapted from any of the projects above.

Architectural lessons — not code — were also taken from the dora-rs project
(Apache-2.0), audited at 1.0.0-rc.4. The AstRS
manifest schema is a deliberate compatible superset of dora's descriptor so
that existing dora graphs migrate mechanically; the implementation is
independent.

Arrow IPC compatibility in `astrs-data` is an independent implementation of
the publicly specified Apache Arrow columnar and IPC formats. Golden test
vectors are generated from Apache Arrow implementations (Apache-2.0) and
checked in as test data only.

## Third-party dependencies

Dependency licenses are enforced by `cargo deny` against the allowlist in
`deny.toml`. Run `cargo deny check licenses` for the current inventory.
