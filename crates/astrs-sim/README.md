# astrs-sim

A deterministic fixed-step simulation harness driving AstRS dataflows: a
2-D kinematic playground powering examples and tutorials.

Testing a robot dataflow against real hardware is slow, unrepeatable and
occasionally expensive. This crate drives the same graph against a
simulated robot instead: differential-drive kinematics, a lidar sensor
raycast against an occupancy-grid map, and an odometry estimate that can
optionally drift from ground truth — all advanced by one fixed
[`Timestep`] per tick, with identical seed + inputs always producing a
bit-identical trajectory.

- **`kinematics`** — `Pose2D`/`Velocity2D` and the closed-form
  `integrate_unicycle` differential-drive integrator: the unicycle
  model's *exact* analytic solution for a constant `(v, w)` command held
  over `dt` (not forward-Euler subdivision), so a run's answer does not
  depend on step size. `DiffDriveGeometry` converts between a body-frame
  command and per-wheel speeds.
- **`grid`** — `Grid`, a 2-D occupancy-grid map loadable from a small
  in-crate text format (`width height resolution` header, then
  `.`/`#` rows), convertible to the wire `std/nav/v1/OccupancyGrid` shape.
- **`lidar`** — `LidarConfig`/`cast_scan`: a configurable-FOV lidar sweep,
  each beam marched cell-by-cell through the grid via the
  Amanatides–Woo ("DDA") fast voxel traversal algorithm.
  `LidarConfig::full_circle` builds a correctly-spaced 360° sweep (no
  duplicated seam beam) — see `LidarConfig::new`'s own docs for why a
  naive `new(n, -PI, PI, ..)` gets a full revolution's spacing wrong.
- **`odometry`** — `OdometryModel`: the robot's own dead-reckoned pose
  estimate, perturbed at the per-wheel level by a seeded Gaussian noise
  model (`sigma = 0.0` is bit-exact to ground truth; a positive sigma
  makes it drift, exactly as a real encoder would).
- **`arm`** — `ArmState`: a separate capability driving `astrs-urdf`'s
  forward kinematics for an articulated arm, publishing its live link
  transforms into an `astrs-tf` `TransformBuffer` — not wired into the
  differential-drive node, since there is no batched-transform wire type
  in the `std` registry yet to publish an arbitrary link count through.
- **`world`** — `World`: the composition of a grid, a robot's ground
  truth and odometry, and a lidar, all advanced together by one
  `World::tick` call per simulated tick. Read this module's docs for the
  sim clock's full contract.
- **`wire`** — conversions between this crate's plain `f64` types and the
  curated `std/geometry/v1`, `std/nav/v1` and `std/sensor/v1` wire message
  types; the only module with an `astrs_data`/`astrs_node_api` dependency
  of its own, so the hot simulation path stays free of wire-encoding
  machinery.
- **`trajectory`** — `TrajectoryHasher`, the FNV-1a fingerprint behind
  `World::trajectory_hash`'s determinism check.

Own PRNG ([`rng::SplitMix64`], the public-domain `splitmix64` algorithm),
hand-rolled specifically so this crate's determinism guarantee never
depends on an upstream RNG crate's algorithm staying fixed across a
semver-compatible release.

## Example

```text
cargo run -p astrs-sim --example quickstart
```

builds a `World`, ticks it 40 times under a constant command, and prints
its ground-truth pose, its odometry estimate, and a lidar sweep — see
[`examples/quickstart.rs`](examples/quickstart.rs) for the full,
independently-runnable walkthrough.

## The node binaries and the example dataflow

Three binaries turn `World` into a runnable AstRS graph — `astrs-sim-node`
(the sim itself: `cmd_vel` in, `odom`/`scan`/`tf` out), `astrs-sim-teleop`
(a deterministic straight-then-arc `cmd_vel` schedule), and
`astrs-sim-logger` (a sink tallying all three outputs). `dataflow.yml`, at
this crate's own root, wires the three together:

```text
cargo build -p astrs-sim
astrs run crates/astrs-sim/dataflow.yml
```

## License

Apache-2.0, part of the [AstRS](https://github.com/cool-japan/astrs)
workspace. See
[`NOTICE.md`](https://github.com/cool-japan/astrs/blob/main/NOTICE.md) for
third-party attribution.
