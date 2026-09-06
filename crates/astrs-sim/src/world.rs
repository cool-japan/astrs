//! [`World`] — the deterministic 2-D kinematic playground: a
//! [`Grid`] map, a differential-drive robot's ground
//! truth and odometry estimate, and a lidar sensor, all advanced together
//! by one fixed [`Timestep`] per [`World::tick`] call.
//!
//! # Sim clock semantics
//!
//! There is exactly one clock: [`World::tick`] advances simulated time by
//! exactly [`World::dt`], once per call, and nothing else moves time
//! forward — no wall-clock read anywhere in this module, no drift-prone
//! accumulation of a floating-point delta (see [`crate::kinematics`]'s own
//! docs on why `dt` is an exact [`Timestep`], not an `f64` seconds value).
//! [`World::tick_count`] is the authoritative "how far has this run
//! gotten" — simulated elapsed time is always `dt.elapsed_nanos_after(tick_count)`
//! (computed fresh, never a running sum a caller could double-advance or
//! skip), and [`World::elapsed_nanos`] is that computation done for you.
//!
//! A caller drives this clock by calling [`World::tick`] exactly once per
//! *tick event* it wants simulated — for `astrs-sim-node` (this crate's
//! own node binary), that is one call per `astrs/timer/millis/N` input
//! event the daemon delivers (§8.4's virtual timer source), which is what
//! "ticks in lockstep from a timer input" means concretely: the daemon's
//! timer wheel is the only thing that ever calls [`World::tick`], and it
//! does so at the fixed rate the manifest's `tick:` port declares — not
//! at whatever rate the node's own event loop happens to spin. A node
//! that received two timer events before finishing work on the first
//! would still only be *correct* to call [`World::tick`] twice (once per
//! event), never to skip one to "catch up" — skipping a tick is exactly
//! the kind of clock-rate-dependent behavior [`crate::kinematics`]'s
//! closed-form integrator (see that module's own docs) was built to make
//! irrelevant to the *simulated* answer, but a skipped tick still means
//! `tick_count` under-reports elapsed simulated time relative to what the
//! wall clock actually did, which matters for anything downstream keying
//! on `tick_count` (this crate's own determinism hash included).
//!
//! # Ground truth vs. odometry, restated at the `World` level
//!
//! [`World::tick`] advances **two** independent pose tracks from the same
//! commanded [`Velocity2D`]: [`World::ground_truth_pose`] (exact unicycle
//! integration, see [`crate::kinematics::integrate_unicycle`], no noise —
//! "what actually happened") and [`World::odometry_pose`] (routed through
//! [`crate::odometry::OdometryModel`]'s per-wheel noise model — "what the
//! robot believes happened"). [`World::scan`] rays out from the former;
//! an `Odometry`/`tf` publisher (see `astrs-sim-node`) reports the latter
//! — see [`crate::lidar`] and [`crate::odometry`]'s own docs for why that
//! split is the point, not an oversight.
//!
//! # Kinematic, not physical
//!
//! Nothing in this module stops a commanded velocity at a wall. The
//! [`Grid`] is consulted only by [`World::scan`] (what
//! the simulated sensor *sees*), never by [`World::tick`] (what the
//! simulated robot *does*) — "kinematic playground" (§5.3) rather than a
//! dynamics/physics simulator, deliberately: adding collision response
//! would mean this crate owning a contact-resolution model with its own
//! correctness surface, for a robotics-dataflow tutorial crate whose job
//! is exercising `scan`/`odom`/`tf`/`cmd_vel` wiring, not validating
//! physics.

use crate::Timestep;
use crate::grid::Grid;
use crate::kinematics::{DiffDriveGeometry, Pose2D, Velocity2D, integrate_unicycle};
use crate::lidar::{LidarConfig, LidarScan, cast_scan};
use crate::odometry::OdometryModel;
use crate::trajectory::TrajectoryHasher;

/// The full deterministic simulation state: a map, a robot's ground truth
/// and odometry estimate, and the sensor configuration used to render a
/// scan on demand.
#[derive(Debug, Clone)]
pub struct World {
    grid: Grid,
    lidar: LidarConfig,
    dt: Timestep,
    ground_truth_pose: Pose2D,
    odometry: OdometryModel,
    last_commanded_velocity: Velocity2D,
    tick_count: u64,
    trajectory: TrajectoryHasher,
}

impl World {
    /// Builds a world at `initial_pose`, ticking by `dt`, with a
    /// differential-drive base described by `geometry`, a lidar described
    /// by `lidar`, and odometry noise scaled by `odom_noise_sigma` and
    /// seeded by `seed` (see [`crate::odometry::OdometryModel::new`]).
    ///
    /// Returns [`None`] exactly when [`crate::odometry::OdometryModel::new`]
    /// would — a non-finite or negative `odom_noise_sigma` — since that is
    /// the only fallible input among this constructor's parameters
    /// (`grid`/`geometry`/`lidar` are already-validated values by the time
    /// they reach here: [`Grid`],
    /// [`DiffDriveGeometry`] and [`LidarConfig`] all reject a malformed
    /// construction at their own boundary).
    #[must_use]
    pub fn new(
        grid: Grid,
        geometry: DiffDriveGeometry,
        lidar: LidarConfig,
        dt: Timestep,
        seed: u64,
        odom_noise_sigma: f64,
        initial_pose: Pose2D,
    ) -> Option<Self> {
        let odometry = OdometryModel::new(geometry, odom_noise_sigma, seed, initial_pose)?;
        let mut trajectory = TrajectoryHasher::new();
        trajectory.absorb_pose(initial_pose);
        trajectory.absorb_pose(initial_pose);
        Some(Self {
            grid,
            lidar,
            dt,
            ground_truth_pose: initial_pose,
            odometry,
            last_commanded_velocity: Velocity2D::ZERO,
            tick_count: 0,
            trajectory,
        })
    }

    /// Advances the world by exactly [`World::dt`] under commanded
    /// velocity `cmd` — see [module docs](self) for the sim clock's full
    /// contract (one call, one tick, always).
    pub fn tick(&mut self, cmd: Velocity2D) {
        self.last_commanded_velocity = cmd;
        self.ground_truth_pose = integrate_unicycle(self.ground_truth_pose, cmd, self.dt);
        self.odometry.step(cmd, self.dt);
        self.tick_count += 1;
        self.trajectory.absorb_pose(self.ground_truth_pose);
        self.trajectory.absorb_pose(self.odometry.pose());
    }

    /// The fixed step this world advances by on every [`World::tick`]
    /// call.
    #[must_use]
    pub const fn dt(&self) -> Timestep {
        self.dt
    }

    /// How many ticks have elapsed since construction.
    #[must_use]
    pub const fn tick_count(&self) -> u64 {
        self.tick_count
    }

    /// Simulated elapsed time since construction, in nanoseconds —
    /// `dt.elapsed_nanos_after(tick_count)`, computed fresh rather than
    /// accumulated (see [module docs](self)). [`None`] only in the
    /// practically-unreachable case of overflowing `u64` nanoseconds (~584
    /// years of simulated time at 1ns steps; see
    /// [`Timestep::elapsed_nanos_after`]'s own docs).
    #[must_use]
    pub const fn elapsed_nanos(&self) -> Option<u64> {
        self.dt.elapsed_nanos_after(self.tick_count)
    }

    /// The exact (noise-free) pose — "what actually happened".
    #[must_use]
    pub const fn ground_truth_pose(&self) -> Pose2D {
        self.ground_truth_pose
    }

    /// The robot's own dead-reckoned pose estimate — "what the robot
    /// believes happened", which drifts from
    /// [`World::ground_truth_pose`] exactly when
    /// [`crate::odometry::OdometryModel`]'s noise is enabled.
    #[must_use]
    pub const fn odometry_pose(&self) -> Pose2D {
        self.odometry.pose()
    }

    /// The (possibly noisy) velocity [`World::tick`] last integrated the
    /// odometry estimate with — what an `Odometry` message's `twist`
    /// field reports.
    #[must_use]
    pub const fn odometry_velocity(&self) -> Velocity2D {
        self.odometry.velocity()
    }

    /// The raw commanded velocity [`World::tick`] was last called with,
    /// before any noise model touched it.
    #[must_use]
    pub const fn last_commanded_velocity(&self) -> Velocity2D {
        self.last_commanded_velocity
    }

    /// The map this world's [`World::scan`] rays out against.
    #[must_use]
    pub const fn grid(&self) -> &Grid {
        &self.grid
    }

    /// The lidar configuration [`World::scan`] uses.
    #[must_use]
    pub const fn lidar_config(&self) -> &LidarConfig {
        &self.lidar
    }

    /// Casts a fresh lidar sweep from the current ground-truth pose (see
    /// [module docs](self) for why ground truth, not the odometry
    /// estimate). Callable at any time, not only right after
    /// [`World::tick`] — it recomputes from the world's current state
    /// rather than caching a stale sweep from the last tick.
    #[must_use]
    pub fn scan(&self) -> LidarScan {
        cast_scan(self.ground_truth_pose, &self.grid, &self.lidar)
    }

    /// A fingerprint of every pose (ground truth and odometry) reached so
    /// far — see [`crate::trajectory`]'s own docs. Two worlds built with
    /// identical construction parameters and driven by an identical
    /// sequence of [`World::tick`] calls always report the same hash;
    /// changing the seed, the noise sigma, or even one bit of one
    /// commanded velocity along the way changes it.
    #[must_use]
    pub const fn trajectory_hash(&self) -> u64 {
        self.trajectory.finish()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn small_room() -> Grid {
        Grid::from_text("6 6 1.0\n######\n#....#\n#....#\n#....#\n#....#\n######\n").unwrap()
    }

    fn build_world(seed: u64, sigma: f64) -> World {
        let geometry = DiffDriveGeometry::new(0.3, 0.05).unwrap();
        let lidar =
            LidarConfig::new(8, 0.0, std::f64::consts::TAU * 7.0 / 8.0, 0.05, 10.0).unwrap();
        let dt = Timestep::from_hz(20).unwrap();
        World::new(
            small_room(),
            geometry,
            lidar,
            dt,
            seed,
            sigma,
            Pose2D::new(3.0, 3.0, 0.0),
        )
        .unwrap()
    }

    /// A deterministic, non-trivial command schedule: varies both linear
    /// and angular speed with the tick index, so a run actually exercises
    /// both integration branches and both wheel speeds — a constant
    /// command would still validate determinism, but only weakly.
    fn command_at(tick: u64) -> Velocity2D {
        #[expect(
            clippy::cast_precision_loss,
            reason = "tick counts in these tests are small; exactness of the f64 conversion is not load-bearing here"
        )]
        let t = tick as f64;
        Velocity2D::new(0.4, 0.3 * (t * 0.2).sin())
    }

    fn run_world(seed: u64, sigma: f64, ticks: u64) -> u64 {
        let mut world = build_world(seed, sigma);
        for tick in 0..ticks {
            world.tick(command_at(tick));
        }
        world.trajectory_hash()
    }

    // ---- Sim clock semantics -----------------------------------------

    #[test]
    fn a_fresh_world_has_ticked_zero_times() {
        let world = build_world(1, 0.0);
        assert_eq!(world.tick_count(), 0);
        assert_eq!(world.elapsed_nanos(), Some(0));
    }

    #[test]
    fn tick_count_and_elapsed_time_advance_by_exactly_one_dt_per_call() {
        let mut world = build_world(1, 0.0);
        let dt_nanos = world.dt().as_nanos();
        for expected_ticks in 1..=5_u64 {
            world.tick(Velocity2D::ZERO);
            assert_eq!(world.tick_count(), expected_ticks);
            assert_eq!(world.elapsed_nanos(), Some(dt_nanos * expected_ticks));
        }
    }

    // ---- Ground truth vs. odometry -------------------------------------

    #[test]
    fn zero_sigma_keeps_odometry_identical_to_ground_truth_across_many_ticks() {
        let mut world = build_world(1, 0.0);
        for tick in 0..100 {
            world.tick(command_at(tick));
        }
        assert_eq!(
            world.ground_truth_pose().x.to_bits(),
            world.odometry_pose().x.to_bits()
        );
        assert_eq!(
            world.ground_truth_pose().y.to_bits(),
            world.odometry_pose().y.to_bits()
        );
    }

    #[test]
    fn a_nonzero_sigma_makes_odometry_diverge_from_ground_truth() {
        let mut world = build_world(1, 0.08);
        for tick in 0..200 {
            world.tick(command_at(tick));
        }
        let dx = world.ground_truth_pose().x - world.odometry_pose().x;
        let dy = world.ground_truth_pose().y - world.odometry_pose().y;
        assert!((dx * dx + dy * dy).sqrt() > 1e-6);
    }

    // ---- scan() ---------------------------------------------------------

    #[test]
    fn scan_reflects_the_current_ground_truth_pose_not_a_stale_one() {
        let mut world = build_world(1, 0.0);
        let first = world.scan();
        // Drive toward one wall for a while.
        for _ in 0..40 {
            world.tick(Velocity2D::new(0.4, 0.0));
        }
        let second = world.scan();
        assert_ne!(
            first.ranges, second.ranges,
            "scan should track the live pose"
        );
    }

    #[test]
    fn scan_uses_the_configured_lidar() {
        let world = build_world(1, 0.0);
        assert_eq!(
            world.scan().ranges.len(),
            world.lidar_config().num_rays() as usize
        );
    }

    // ---- The determinism hash test -------------------------------------

    /// The headline promise: identical seed, identical (deterministic)
    /// command sequence -> bit-identical trajectory, checked via one
    /// `assert_eq!` on the hash rather than a pose-by-pose walk.
    #[test]
    fn identical_seed_and_inputs_produce_an_identical_trajectory_hash() {
        let a = run_world(7, 0.05, 300);
        let b = run_world(7, 0.05, 300);
        assert_eq!(a, b);
    }

    /// The hash is actually sensitive to the run, not a constant: two
    /// different seeds (with noise enabled, so the seed has somewhere to
    /// matter) produce different hashes.
    #[test]
    fn different_seeds_produce_different_trajectory_hashes() {
        let a = run_world(7, 0.05, 300);
        let b = run_world(8, 0.05, 300);
        assert_ne!(a, b);
    }

    /// The hash is also sensitive to the noise scale alone, holding the
    /// seed and command sequence fixed.
    #[test]
    fn different_noise_sigmas_produce_different_trajectory_hashes() {
        let a = run_world(7, 0.05, 300);
        let b = run_world(7, 0.10, 300);
        assert_ne!(a, b);
    }

    /// And to a difference in the command sequence itself — a stand-in
    /// for "this is really hashing the trajectory, not e.g. only the
    /// seed".
    #[test]
    fn a_different_command_sequence_produces_a_different_trajectory_hash() {
        let mut a = build_world(7, 0.0);
        let mut b = build_world(7, 0.0);
        for tick in 0..50 {
            a.tick(command_at(tick));
            b.tick(command_at(tick + 1)); // shifted schedule
        }
        assert_ne!(a.trajectory_hash(), b.trajectory_hash());
    }

    #[test]
    fn a_shorter_run_of_the_same_schedule_has_a_different_hash_than_a_longer_one() {
        assert_ne!(run_world(7, 0.0, 10), run_world(7, 0.0, 11));
    }
}
