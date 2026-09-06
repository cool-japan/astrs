//! A deterministic fixed-step simulation harness driving AstRS dataflows.
//!
//! Testing a robot dataflow against real hardware is slow, unrepeatable and
//! occasionally expensive. This crate drives the same graph against a
//! simulated robot instead: an [`astrs_urdf`] model supplies the kinematics, a
//! fixed-step integrator advances it, [`astrs_tf`] receives the resulting
//! transforms, and ordinary [`astrs_node_api`] nodes see sensor and command
//! topics indistinguishable from the real thing — carrying the same
//! [`astrs_data`] columnar payloads.
//!
//! # Determinism is the whole point
//!
//! A simulated run is only useful as a regression test if it produces the
//! same outputs every time, on every machine. That rules out a wall-clock
//! -driven loop and it rules out accumulating a floating-point `dt`: after an
//! hour at 1 kHz, `3_600_000 × 0.001` has drifted, while `3_600_000 ×
//! 1_000_000 ns` has not. [`Timestep`] is therefore an exact integer
//! nanosecond quantity, and simulated time is always `step × count` rather
//! than a running sum.
//!
//! ```
//! use astrs_sim::Timestep;
//!
//! let step = Timestep::from_hz(500).expect("500 Hz divides one second exactly");
//! assert_eq!(step.as_nanos(), 2_000_000);
//!
//! // One simulated hour, exactly — computed, never accumulated.
//! let one_hour = step.elapsed_nanos_after(500 * 3600);
//! assert_eq!(one_hour, Some(3_600_000_000_000));
//! ```
//!
//! # Module map
//!
//! - [`grid`] — [`grid::Grid`], the occupancy-grid map (loadable from a
//!   small in-crate text format).
//! - [`kinematics`] — [`kinematics::Pose2D`]/[`kinematics::Velocity2D`]
//!   and the closed-form [`kinematics::integrate_unicycle`] differential
//!   -drive integrator both [`world::World`]'s ground truth and
//!   [`odometry::OdometryModel`] build on.
//! - [`lidar`] — [`lidar::LidarConfig`] and [`lidar::cast_scan`], a
//!   DDA/Amanatides–Woo raycast against a [`grid::Grid`].
//! - [`odometry`] — [`odometry::OdometryModel`], the robot's own
//!   (optionally noisy) dead-reckoned pose estimate, kept deliberately
//!   separate from [`World`]'s ground truth.
//! - [`arm`] — [`arm::ArmState`], a *separate* capability (articulated
//!   arms via [`astrs_urdf`] forward kinematics) this crate also
//!   provides, not wired into the differential-drive node — see that
//!   module's own docs for why.
//! - [`trajectory`] — [`trajectory::TrajectoryHasher`], the fingerprint
//!   behind [`World::trajectory_hash`]'s determinism check.
//! - [`world`] — [`World`] itself: the composition of a grid, a robot's
//!   ground truth and odometry, and a lidar, all advanced together by one
//!   fixed [`Timestep`] per [`World::tick`] call. Read this module's docs
//!   first for the sim clock's full contract.
//! - [`wire`] — conversions between this crate's pure `f64`/[`kinematics::Pose2D`]/
//!   [`kinematics::Velocity2D`] types and the curated `std` wire message
//!   types ([`astrs_node_api::message`]); the only module in this crate
//!   with an `astrs_data`/`astrs_node_api` dependency of its own.
//!
//! # The node binaries and the example dataflow
//!
//! Three binaries turn [`World`] into a runnable AstRS graph:
//! `astrs-sim-node` (the sim itself: `cmd_vel` in, `odom`/`scan`/`tf`
//! out), `astrs-sim-teleop` (a deterministic straight-then-arc `cmd_vel`
//! schedule), and `astrs-sim-logger` (a sink that tallies all three
//! outputs). `dataflow.yml`, at this crate's own root, wires the three
//! together and is this crate's worked example:
//!
//! ```text
//! cargo build -p astrs-sim
//! astrs run crates/astrs-sim/dataflow.yml
//! ```
//!
//! See `astrs-sim-node`'s own doc comment for the tick/`cmd_vel` wiring in
//! full, and [`World`]'s docs for the sim clock semantics that binary
//! implements verbatim.

use std::num::NonZeroU64;

pub mod arm;
pub mod error;
pub mod grid;
pub mod kinematics;
pub mod lidar;
pub mod odometry;
pub mod rng;
pub mod trajectory;
pub mod wire;
pub mod world;

pub use error::{Result, SimError};
pub use world::World;

/// Nanoseconds in one second.
const NANOS_PER_SEC: u64 = 1_000_000_000;

/// A fixed simulation step, held as an exact non-zero nanosecond count.
///
/// Integer nanoseconds rather than a floating-point `dt`: a fixed-step
/// integrator that accumulates `f64` seconds drifts, and a drifting clock
/// makes a simulated run unusable as a regression test. Non-zero because a
/// zero step advances nothing and would make every "how long until…" query
/// diverge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestep(NonZeroU64);

impl Timestep {
    /// A step of exactly `nanos` nanoseconds, or [`None`] if `nanos` is zero.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_sim::Timestep;
    ///
    /// assert!(Timestep::from_nanos(1).is_some());
    /// assert!(Timestep::from_nanos(0).is_none());
    /// ```
    #[must_use]
    pub const fn from_nanos(nanos: u64) -> Option<Self> {
        match NonZeroU64::new(nanos) {
            Some(nanos) => Some(Self(nanos)),
            None => None,
        }
    }

    /// A step of exactly `1/hz` seconds.
    ///
    /// [`None`] when `hz` is zero, or when one second does not divide into
    /// `hz` equal whole nanoseconds — 60 Hz, for instance, is
    /// `16_666_666.67 ns` and has no exact integer representation. That is
    /// rejected rather than rounded: a silently rounded step is exactly the
    /// drift this type exists to prevent. Callers who genuinely want such a
    /// rate can pick the rounded value deliberately with
    /// [`Timestep::from_nanos`].
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_sim::Timestep;
    ///
    /// assert_eq!(Timestep::from_hz(1000).map(Timestep::as_nanos), Some(1_000_000));
    /// assert_eq!(Timestep::from_hz(0), None);
    /// assert_eq!(Timestep::from_hz(60), None); // 16_666_666.67 ns is not exact
    /// ```
    #[must_use]
    pub const fn from_hz(hz: u64) -> Option<Self> {
        if hz == 0 || !NANOS_PER_SEC.is_multiple_of(hz) {
            return None;
        }
        Self::from_nanos(NANOS_PER_SEC / hz)
    }

    /// This step's length in nanoseconds.
    #[must_use]
    pub const fn as_nanos(self) -> u64 {
        self.0.get()
    }

    /// This step's length in seconds, for the rare consumer that genuinely
    /// wants a float (a physics constant, a plot axis). Never use this to
    /// advance simulated time — that is what
    /// [`Timestep::elapsed_nanos_after`] is for.
    #[must_use]
    pub fn as_secs_f64(self) -> f64 {
        self.as_nanos() as f64 / NANOS_PER_SEC as f64
    }

    /// This step's rate in hertz.
    #[must_use]
    pub fn hz(self) -> f64 {
        NANOS_PER_SEC as f64 / self.as_nanos() as f64
    }

    /// Simulated time after exactly `steps` steps, in nanoseconds.
    ///
    /// [`None`] on overflow — roughly 584 years at one-nanosecond steps.
    /// Computed as a single multiplication rather than a running sum, so the
    /// answer is identical regardless of how the caller got there.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_sim::Timestep;
    ///
    /// let step = Timestep::from_hz(100).expect("100 Hz is exact");
    /// assert_eq!(step.elapsed_nanos_after(0), Some(0));
    /// assert_eq!(step.elapsed_nanos_after(100), Some(1_000_000_000));
    /// assert_eq!(step.elapsed_nanos_after(u64::MAX), None);
    /// ```
    #[must_use]
    pub const fn elapsed_nanos_after(self, steps: u64) -> Option<u64> {
        self.as_nanos().checked_mul(steps)
    }

    /// How many whole steps fit into `nanos` of simulated time.
    #[must_use]
    pub const fn steps_in(self, nanos: u64) -> u64 {
        nanos / self.as_nanos()
    }
}

impl std::fmt::Display for Timestep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ns", self.as_nanos())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn a_zero_step_is_rejected() {
        assert_eq!(Timestep::from_nanos(0), None);
        assert_eq!(Timestep::from_hz(0), None);
    }

    #[test]
    fn exactly_divisible_rates_are_accepted() {
        for (hz, nanos) in [
            (1_u64, 1_000_000_000_u64),
            (2, 500_000_000),
            (100, 10_000_000),
            (500, 2_000_000),
            (1000, 1_000_000),
            (1_000_000_000, 1),
        ] {
            let step = Timestep::from_hz(hz).unwrap_or_else(|| panic!("{hz} Hz must be exact"));
            assert_eq!(step.as_nanos(), nanos, "{hz} Hz");
        }
    }

    #[test]
    fn inexact_rates_are_rejected_rather_than_rounded() {
        // Silently rounding these is precisely the drift `Timestep` exists
        // to prevent.
        for hz in [3_u64, 7, 60, 240, 1_000_000_001] {
            assert_eq!(Timestep::from_hz(hz), None, "{hz} Hz must be rejected");
        }
    }

    #[test]
    fn elapsed_time_is_multiplied_never_accumulated() {
        let step = Timestep::from_hz(1000).unwrap();
        // One simulated hour at 1 kHz.
        assert_eq!(step.elapsed_nanos_after(3_600_000), Some(3_600_000_000_000));
        assert_eq!(step.elapsed_nanos_after(0), Some(0));
    }

    #[test]
    fn elapsed_time_reports_overflow_rather_than_wrapping() {
        let step = Timestep::from_nanos(1_000_000).unwrap();
        assert_eq!(step.elapsed_nanos_after(u64::MAX), None);
    }

    #[test]
    fn steps_in_is_the_inverse_of_elapsed_for_whole_multiples() {
        let step = Timestep::from_hz(100).unwrap();
        for count in [0_u64, 1, 17, 100_000] {
            let nanos = step.elapsed_nanos_after(count).unwrap();
            assert_eq!(step.steps_in(nanos), count);
        }
        // A partial step does not count.
        assert_eq!(step.steps_in(step.as_nanos() - 1), 0);
    }

    #[test]
    fn float_views_agree_with_the_exact_integer_value() {
        let step = Timestep::from_hz(500).unwrap();
        assert!((step.as_secs_f64() - 0.002).abs() < 1e-12);
        assert!((step.hz() - 500.0).abs() < 1e-9);
    }

    #[test]
    fn display_names_the_nanosecond_length() {
        assert_eq!(
            Timestep::from_nanos(2_000_000).unwrap().to_string(),
            "2000000 ns"
        );
    }

    /// The committed `dataflow.yml` parses, validates, wires every typed
    /// edge, and names this dataflow — the same "manifest actually
    /// matches the code" gate `examples/tf-broadcast`'s own
    /// `the_committed_manifest_parses_and_validates` test runs, applied to
    /// this crate's own example.
    #[test]
    fn the_committed_dataflow_manifest_parses_and_validates() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("dataflow.yml");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("std/nav/v1/Odometry"), "{text}");
        assert!(text.contains("std/sensor/v1/LaserScan"), "{text}");
        assert!(text.contains("std/geometry/v1/Transform"), "{text}");
        assert!(text.contains("std/geometry/v1/Twist"), "{text}");

        let manifest = astrs_manifest::Manifest::from_yaml_file(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        manifest
            .validate()
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_eq!(manifest.name.as_deref(), Some("astrs-sim"));
        assert_eq!(manifest.nodes.len(), 3);
        let ids: Vec<&str> = manifest.nodes.iter().map(|node| node.id.as_str()).collect();
        assert_eq!(ids, ["teleop", "sim", "logger"]);
    }
}
