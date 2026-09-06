//! [`OdometryModel`] — the robot's own (possibly noisy) belief about its
//! pose, kept deliberately separate from [`crate::World`]'s ground truth.
//!
//! # Why noise lives at the wheel, not the body-frame command
//!
//! Real wheel odometry drifts because of what happens at each *wheel*
//! independently — a slightly under-inflated tyre, a patch of wheel slip,
//! encoder quantization — not because the robot's overall `(v, w)` command
//! was somehow noisy (the controller commanded exactly what it commanded;
//! the ground truth in [`crate::World`] realizes it exactly, with no
//! actuator noise modeled at that layer at all). So this model perturbs
//! each of [`crate::kinematics::DiffDriveGeometry::to_wheel_speeds`]'s two
//! outputs independently and converts back — see [`OdometryModel::step`].
//!
//! # `sigma == 0.0` still takes its own path — deliberately, for a
//! # precision reason, not merely a performance one
//!
//! [`OdometryModel::step`] always draws two Gaussian samples from its own
//! [`SplitMix64`] every tick, regardless of `sigma` — that part genuinely
//! has no branch, so the generator's state consumption stays independent
//! of `sigma`, and toggling `sigma` between two runs of the same seed
//! never reorders which draw lands on which tick.
//!
//! *Which velocity gets integrated*, though, does branch on `sigma`, and
//! not merely as an optimization. The tempting argument — "at `sigma ==
//! 0.0`, `wheel_speed * (1.0 + 0.0 * gaussian)` is `wheel_speed * 1.0`,
//! and multiplying by `1.0` is an exact IEEE 754 identity, so the round
//! trip through [`DiffDriveGeometry::to_wheel_speeds`]/[`DiffDriveGeometry::from_wheel_speeds`]
//! must come back bit-exact" — is wrong, and this crate's own first draft
//! shipped it and had a failing test prove it wrong: each wheel speed
//! individually survives the `* 1.0` exactly, but
//! `from_wheel_speeds(to_wheel_speeds(v, w))` still computes `(left +
//! right) / 2.0` and `(right - left) / wheel_base` from those two
//! independently-rounded wheel speeds, and `(v - h) + (v + h)` is not
//! guaranteed to equal `2 * v` bit-for-bit once `v` and `h` are both
//! inexact `f64` values — the two intermediate roundings do not always
//! cancel. The observed drift was a single ULP, immaterial for anything
//! this crate's noise model is *for*, but a real, checkable discrepancy
//! all the same, and "close" is not what [`OdometryModel::step`]'s
//! exactness claim promises.
//!
//! The fix is to never take the round trip at all when there is nothing
//! for it to accomplish: at `sigma == 0.0`, this module's private
//! velocity-perturbation helper returns the *commanded* velocity
//! directly, so [`OdometryModel::step`] calls
//! [`crate::kinematics::integrate_unicycle`] with the exact same argument
//! [`crate::World`]'s ground truth does — not merely an argument that
//! *should* reduce to the same value. This module's own test suite checks
//! that claim bit-for-bit, not merely approximately.

use crate::Timestep;
use crate::kinematics::{DiffDriveGeometry, Pose2D, Velocity2D, WheelSpeeds, integrate_unicycle};
use crate::rng::SplitMix64;

/// A robot's dead-reckoned pose estimate, integrated from commanded
/// velocities through a per-wheel Gaussian noise model.
#[derive(Debug, Clone)]
pub struct OdometryModel {
    geometry: DiffDriveGeometry,
    noise_sigma: f64,
    rng: SplitMix64,
    pose: Pose2D,
    last_velocity: Velocity2D,
}

impl OdometryModel {
    /// Builds a model seeded at `initial_pose`, with per-wheel noise drawn
    /// from a generator seeded by `seed` and scaled by `noise_sigma`
    /// (dimensionless: a fraction of each wheel's commanded speed — `0.02`
    /// means "about 2% wheel-speed noise", a plausible real-encoder
    /// figure).
    ///
    /// Returns [`None`] for a non-finite or negative `noise_sigma` — a
    /// negative sigma is not "less noise than zero", it is meaningless,
    /// and this crate's house style (see `astrs_urdf`'s own construction
    /// checks) rejects that kind of input at the boundary rather than
    /// letting it silently produce a nonsensical (but not obviously wrong
    /// -looking) result downstream.
    #[must_use]
    pub fn new(
        geometry: DiffDriveGeometry,
        noise_sigma: f64,
        seed: u64,
        initial_pose: Pose2D,
    ) -> Option<Self> {
        if !noise_sigma.is_finite() || noise_sigma < 0.0 {
            return None;
        }
        Some(Self {
            geometry,
            noise_sigma,
            rng: SplitMix64::new(seed),
            pose: initial_pose,
            last_velocity: Velocity2D::ZERO,
        })
    }

    /// This model's configured noise scale.
    #[must_use]
    pub const fn noise_sigma(&self) -> f64 {
        self.noise_sigma
    }

    /// The current pose estimate.
    #[must_use]
    pub const fn pose(&self) -> Pose2D {
        self.pose
    }

    /// The last effective (possibly noisy) velocity [`OdometryModel::step`]
    /// integrated — what an `Odometry` message's `twist` field reports,
    /// distinct from the *commanded* velocity a caller passed in.
    #[must_use]
    pub const fn velocity(&self) -> Velocity2D {
        self.last_velocity
    }

    /// Advances the estimate by `dt` under commanded velocity `commanded`,
    /// and returns the new pose (also available afterwards via
    /// [`OdometryModel::pose`]).
    pub fn step(&mut self, commanded: Velocity2D, dt: Timestep) -> Pose2D {
        let effective = self.effective_velocity(commanded);
        self.last_velocity = effective;
        self.pose = integrate_unicycle(self.pose, effective, dt);
        self.pose
    }

    /// Perturbs `commanded` at the wheel-speed level and converts back, or
    /// — at `noise_sigma == 0.0` — returns `commanded` untouched. See
    /// [module docs](self) for why the RNG draw happens unconditionally
    /// (two draws, every call) while the wheel-speed round trip itself
    /// does not: it is the only way to make [`OdometryModel::step`]'s
    /// bit-exactness claim at `noise_sigma == 0.0` actually true, rather
    /// than merely algebraically true.
    fn effective_velocity(&mut self, commanded: Velocity2D) -> Velocity2D {
        let left_noise = self.rng.next_gaussian();
        let right_noise = self.rng.next_gaussian();
        if self.noise_sigma == 0.0 {
            return commanded;
        }
        let wheels = self.geometry.to_wheel_speeds(commanded);
        let noisy = WheelSpeeds::new(
            wheels.left * (1.0 + self.noise_sigma * left_noise),
            wheels.right * (1.0 + self.noise_sigma * right_noise),
        );
        self.geometry.from_wheel_speeds(noisy)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn geometry() -> DiffDriveGeometry {
        DiffDriveGeometry::new(0.4, 0.05).unwrap()
    }

    #[test]
    fn a_negative_or_non_finite_sigma_is_rejected() {
        assert!(OdometryModel::new(geometry(), -0.01, 1, Pose2D::ORIGIN).is_none());
        assert!(OdometryModel::new(geometry(), f64::NAN, 1, Pose2D::ORIGIN).is_none());
        assert!(OdometryModel::new(geometry(), f64::INFINITY, 1, Pose2D::ORIGIN).is_none());
    }

    #[test]
    fn a_zero_sigma_is_accepted_and_is_the_exact_mode() {
        assert!(OdometryModel::new(geometry(), 0.0, 1, Pose2D::ORIGIN).is_some());
    }

    /// The headline exactness claim from this module's own docs, made
    /// precise: `to_bits()` equal, not merely "close" — the noise
    /// application at `sigma == 0.0` must be a genuine no-op at the bit
    /// level, not an approximation that happens to be small.
    #[test]
    fn zero_sigma_odometry_matches_ground_truth_kinematics_bit_for_bit() {
        let dt = Timestep::from_hz(50).unwrap();
        let commands = [
            Velocity2D::new(1.0, 0.0),
            Velocity2D::new(0.5, 0.8),
            Velocity2D::new(-0.3, -1.2),
            Velocity2D::new(0.0, 0.0),
            Velocity2D::new(2.0, 0.1),
        ];

        let mut odometry = OdometryModel::new(geometry(), 0.0, 42, Pose2D::ORIGIN).unwrap();
        let mut ground_truth = Pose2D::ORIGIN;
        for &cmd in &commands {
            let estimated = odometry.step(cmd, dt);
            ground_truth = integrate_unicycle(ground_truth, cmd, dt);
            assert_eq!(estimated.x.to_bits(), ground_truth.x.to_bits());
            assert_eq!(estimated.y.to_bits(), ground_truth.y.to_bits());
            assert_eq!(estimated.theta.to_bits(), ground_truth.theta.to_bits());
        }
        // ... and the reported twist is exactly the commanded one too.
        assert_eq!(odometry.velocity(), *commands.last().unwrap());
    }

    #[test]
    fn a_nonzero_sigma_makes_the_estimate_diverge_from_ground_truth() {
        let dt = Timestep::from_hz(50).unwrap();
        let mut odometry = OdometryModel::new(geometry(), 0.05, 7, Pose2D::ORIGIN).unwrap();
        let mut ground_truth = Pose2D::ORIGIN;
        let cmd = Velocity2D::new(1.0, 0.3);
        for _ in 0..200 {
            odometry.step(cmd, dt);
            ground_truth = integrate_unicycle(ground_truth, cmd, dt);
        }
        let drift = ((odometry.pose().x - ground_truth.x).powi(2)
            + (odometry.pose().y - ground_truth.y).powi(2))
        .sqrt();
        assert!(drift > 1e-6, "expected measurable drift, got {drift}");
    }

    #[test]
    fn the_same_seed_and_commands_produce_the_same_noisy_trajectory() {
        let dt = Timestep::from_hz(50).unwrap();
        let cmd = Velocity2D::new(0.8, -0.4);
        let run = |seed: u64| {
            let mut model = OdometryModel::new(geometry(), 0.1, seed, Pose2D::ORIGIN).unwrap();
            for _ in 0..50 {
                model.step(cmd, dt);
            }
            model.pose()
        };
        let a = run(123);
        let b = run(123);
        assert_eq!(a.x.to_bits(), b.x.to_bits());
        assert_eq!(a.y.to_bits(), b.y.to_bits());
        assert_eq!(a.theta.to_bits(), b.theta.to_bits());
    }

    #[test]
    fn different_seeds_produce_different_noisy_trajectories() {
        let dt = Timestep::from_hz(50).unwrap();
        let cmd = Velocity2D::new(0.8, -0.4);
        let run = |seed: u64| {
            let mut model = OdometryModel::new(geometry(), 0.1, seed, Pose2D::ORIGIN).unwrap();
            for _ in 0..50 {
                model.step(cmd, dt);
            }
            model.pose()
        };
        let a = run(1);
        let b = run(2);
        assert_ne!(a.x.to_bits(), b.x.to_bits());
    }

    #[test]
    fn noise_sigma_and_pose_accessors_report_the_constructed_state() {
        let model = OdometryModel::new(geometry(), 0.03, 9, Pose2D::new(1.0, 2.0, 0.5)).unwrap();
        assert_eq!(model.noise_sigma(), 0.03);
        assert_eq!(model.pose(), Pose2D::new(1.0, 2.0, 0.5));
        assert_eq!(model.velocity(), Velocity2D::ZERO);
    }
}
