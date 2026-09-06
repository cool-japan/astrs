//! `astrs-sim-node` — the deterministic differential-drive simulator this
//! crate's node convention centers on (see `astrs_sim`'s own crate docs
//! for the full picture, and `astrs_sim::World`'s docs for the sim clock
//! contract this binary implements verbatim).
//!
//! ```text
//!   astrs/timer/millis/N ──► tick ──► [astrs-sim-node] ──odom──►
//!                cmd_vel ───────────►                  ├─scan──►
//!                                                       └─tf────►
//! ```
//!
//! One [`astrs_sim::World::tick`] call per `tick` input event — no more,
//! no less (`astrs_sim::World`'s own docs on why that is the whole sim
//! clock contract). `cmd_vel` updates the commanded velocity the *next*
//! tick integrates with (a zero-order hold: the world keeps integrating
//! the most recently received command every tick until a new one
//! arrives, exactly like a real diff-drive base's motor controller
//! between commands) rather than triggering motion itself.

use std::process::ExitCode;

use astrs_node_api::{Event, Node};
use astrs_sim::grid::Grid;
use astrs_sim::kinematics::{DiffDriveGeometry, Pose2D, Velocity2D};
use astrs_sim::lidar::LidarConfig;
use astrs_sim::wire::{
    laser_scan_message, odometry_message, pose2d_to_transform, twist_to_velocity,
};
use astrs_sim::{Timestep, World};

/// The timer input, as named in `dataflow.yml`.
const TICK_PORT: &str = "tick";
/// The commanded-velocity input.
const CMD_VEL_PORT: &str = "cmd_vel";
/// The odometry estimate output.
const ODOM_PORT: &str = "odom";
/// The lidar sweep output.
const SCAN_PORT: &str = "scan";
/// The dynamic `odom -> base_link` transform output.
const TF_PORT: &str = "tf";

/// Overrides the odometry noise generator's seed.
const ENV_SEED: &str = "ASTRS_SIM_SEED";
/// Overrides the per-wheel odometry noise scale (`0.0` = exact).
const ENV_NOISE_SIGMA: &str = "ASTRS_SIM_ODOM_NOISE_SIGMA";
/// Overrides how many ticks this node runs for before finishing.
const ENV_TICKS: &str = "ASTRS_SIM_TICKS";
/// Overrides the map, given as the raw text `astrs_sim::grid::Grid::from_text`
/// parses — see that function's own docs for the format.
const ENV_MAP: &str = "ASTRS_SIM_MAP";

/// The default odometry noise seed.
const DEFAULT_SEED: u64 = 42;
/// The default (exact, noise-free) odometry mode.
const DEFAULT_NOISE_SIGMA: f64 = 0.0;
/// The default tick budget: 400 ticks at [`TICK_HZ`] is 20 simulated
/// seconds.
const DEFAULT_TICKS: u64 = 400;
/// The fixed simulated tick rate this node advances at — must match the
/// `astrs/timer/millis/N` port `dataflow.yml` wires `tick` to
/// (`1000 / TICK_HZ` milliseconds).
const TICK_HZ: u64 = 20;
/// The differential-drive base's track width, in metres.
const WHEEL_BASE: f64 = 0.30;
/// Each wheel's rolling radius, in metres.
const WHEEL_RADIUS: f64 = 0.05;
/// How many beams one lidar sweep casts.
const LIDAR_RAYS: u32 = 36;
/// The lidar's shortest reportable range, in metres.
const LIDAR_RANGE_MIN: f64 = 0.05;
/// The lidar's longest reportable range, in metres.
const LIDAR_RANGE_MAX: f64 = 10.0;
/// The robot's starting pose in the default map — the center of a free
/// cell, confirmed clear of [`DEFAULT_MAP`]'s own interior pillar (this
/// file's own test suite checks it stays that way).
const DEFAULT_INITIAL_POSE: Pose2D = Pose2D::new(1.5, 1.5, 0.0);

/// A small room with an interior pillar, `0.5m` cells — this node's map
/// when [`ENV_MAP`] names none of its own. See
/// [`astrs_sim::grid::Grid::from_text`] for the format.
const DEFAULT_MAP: &str = "\
14 10 0.5
##############
#............#
#............#
#....##......#
#....##......#
#............#
#............#
#............#
#............#
##############
";

fn main() -> ExitCode {
    match run() {
        Ok(ticks) => {
            println!("astrs-sim-node: simulated {ticks} ticks");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("astrs-sim-node: {error}");
            ExitCode::FAILURE
        }
    }
}

/// How many ticks this run should simulate before finishing.
fn tick_budget(raw: Option<&str>) -> u64 {
    raw.and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|ticks| *ticks > 0)
        .unwrap_or(DEFAULT_TICKS)
}

/// The odometry noise generator's seed.
fn seed_from_env(raw: Option<&str>) -> u64 {
    raw.and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_SEED)
}

/// The per-wheel odometry noise scale — a value the manifest cannot
/// express (unparseable, negative, non-finite) falls back to the default
/// exact mode rather than failing the node.
fn noise_sigma_from_env(raw: Option<&str>) -> f64 {
    raw.and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|sigma| sigma.is_finite() && *sigma >= 0.0)
        .unwrap_or(DEFAULT_NOISE_SIGMA)
}

/// The map text this run uses: [`ENV_MAP`]'s value if it parses, else
/// [`DEFAULT_MAP`] — an unparseable override falls back rather than
/// failing the node, matching every other env override here.
fn map_text_from_env(raw: Option<String>) -> String {
    match raw {
        Some(text) if Grid::from_text(&text).is_ok() => text,
        _ => DEFAULT_MAP.to_owned(),
    }
}

/// Builds the world this run simulates from environment overrides (or
/// their defaults), along with the resolved seed/sigma (for logging —
/// [`World`] itself exposes neither, see that type's own docs on what it
/// does and does not carry).
///
/// # Errors
///
/// [`map_text_from_env`] already falls back to [`DEFAULT_MAP`] for
/// anything unparseable, so the only realistic way this returns an error
/// is a corrupted build of this binary's own compile-time constants — see
/// the function body's own comment.
fn build_world() -> Result<(World, u64, f64), Box<dyn std::error::Error>> {
    let seed = seed_from_env(std::env::var(ENV_SEED).ok().as_deref());
    let sigma = noise_sigma_from_env(std::env::var(ENV_NOISE_SIGMA).ok().as_deref());
    let map_text = map_text_from_env(std::env::var(ENV_MAP).ok());

    // `map_text_from_env` only ever returns text `Grid::from_text` has
    // already confirmed parses (or `DEFAULT_MAP`, parsed once by this
    // file's own test suite) — see that function's own docs.
    // Every `.ok_or(..)?` below reports a genuine (if, given this file's
    // own tests of these exact constants, practically unreachable)
    // construction failure as an ordinary error `run` propagates to
    // `main`'s exit code — never a panic. See
    // `tests::the_compile_time_geometry_and_lidar_constants_are_valid`
    // and `tests::the_default_map_parses` for what actually guards these
    // constants day to day.
    let grid = Grid::from_text(&map_text)?;
    let geometry = DiffDriveGeometry::new(WHEEL_BASE, WHEEL_RADIUS)
        .ok_or("astrs-sim-node's own WHEEL_BASE/WHEEL_RADIUS constants are invalid")?;
    // `LidarConfig::full_circle`, not `LidarConfig::new(.., -PI, PI, ..)`:
    // this sensor spans the entire circle, and `new`'s inclusive-endpoint
    // convention would make beam 0 and the last beam the same physical
    // bearing — see `LidarConfig::new`'s own docs on exactly this pitfall.
    let lidar = LidarConfig::full_circle(LIDAR_RAYS, LIDAR_RANGE_MIN, LIDAR_RANGE_MAX)
        .ok_or("astrs-sim-node's own LIDAR_* constants are invalid")?;
    let dt = Timestep::from_hz(TICK_HZ)
        .ok_or("astrs-sim-node's own TICK_HZ constant does not divide 1s exactly")?;

    let world = World::new(grid, geometry, lidar, dt, seed, sigma, DEFAULT_INITIAL_POSE)
        .ok_or("astrs-sim-node's own noise sigma was rejected after already being validated")?;
    Ok((world, seed, sigma))
}

/// Runs the sim loop to completion, returning how many ticks were
/// simulated.
fn run() -> Result<u64, Box<dyn std::error::Error>> {
    let budget = tick_budget(std::env::var(ENV_TICKS).ok().as_deref());
    let (mut world, seed, sigma) = build_world()?;

    let (mut node, mut events) = Node::init_from_env()?;
    let mut odom_out = node.output::<astrs_node_api::message::Odometry>(ODOM_PORT)?;
    let mut scan_out = node.output::<astrs_node_api::message::LaserScan>(SCAN_PORT)?;
    let mut tf_out = node.output::<astrs_node_api::message::Transform>(TF_PORT)?;
    node.log_info(format!(
        "astrs-sim-node up: {budget} ticks at {TICK_HZ}Hz, seed={seed}, noise_sigma={sigma}"
    ));

    let mut commanded = Velocity2D::ZERO;
    let mut ticks = 0_u64;
    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, data, .. } if id.as_str() == CMD_VEL_PORT => {
                let twist: astrs_node_api::message::Twist = data.view()?;
                commanded = twist_to_velocity(&twist);
            }
            Event::Input { id, meta, .. } if id.as_str() == TICK_PORT => {
                world.tick(commanded);
                let dt_secs = world.dt().as_secs_f64();
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "a scan's own scan_time field is f32 by std/sensor/v1's own wire contract"
                )]
                let scan_time = dt_secs as f32;

                odom_out.send(
                    odometry_message(world.odometry_pose(), world.odometry_velocity()),
                    meta.follow(),
                )?;
                scan_out.send(laser_scan_message(&world.scan(), scan_time), meta.follow())?;
                tf_out.send(pose2d_to_transform(world.odometry_pose()), meta.follow())?;

                ticks += 1;
                if ticks >= budget {
                    break;
                }
            }
            Event::Stop(cause) => {
                node.log_info(format!("stopping after {ticks} ticks: {cause}"));
                break;
            }
            _ => {}
        }
    }

    odom_out.close()?;
    scan_out.close()?;
    tf_out.close()?;
    node.log_info(format!(
        "simulated {ticks} ticks; trajectory_hash={:#x}",
        world.trajectory_hash()
    ));
    Ok(ticks)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn tick_budget_falls_back_to_the_default_for_unusable_input() {
        assert_eq!(tick_budget(Some("50")), 50);
        for raw in [None, Some(""), Some("lots"), Some("0"), Some("-1")] {
            assert_eq!(tick_budget(raw), DEFAULT_TICKS, "{raw:?}");
        }
    }

    #[test]
    fn seed_from_env_falls_back_to_the_default_for_unusable_input() {
        assert_eq!(seed_from_env(Some("7")), 7);
        for raw in [None, Some(""), Some("nope"), Some("-1")] {
            assert_eq!(seed_from_env(raw), DEFAULT_SEED, "{raw:?}");
        }
    }

    #[test]
    fn noise_sigma_from_env_falls_back_to_the_default_for_unusable_input() {
        assert_eq!(noise_sigma_from_env(Some("0.05")), 0.05);
        for raw in [
            None,
            Some(""),
            Some("nope"),
            Some("-0.1"),
            Some("nan"),
            Some("inf"),
        ] {
            assert_eq!(noise_sigma_from_env(raw), DEFAULT_NOISE_SIGMA, "{raw:?}");
        }
    }

    #[test]
    fn map_text_from_env_falls_back_to_the_default_for_unparseable_text() {
        assert_eq!(map_text_from_env(None), DEFAULT_MAP);
        assert_eq!(map_text_from_env(Some("not a map".to_owned())), DEFAULT_MAP);
        let custom = "2 1 1.0\n.#\n".to_owned();
        assert_eq!(map_text_from_env(Some(custom.clone())), custom);
    }

    #[test]
    fn the_default_map_parses() {
        Grid::from_text(DEFAULT_MAP).unwrap();
    }

    #[test]
    fn the_default_initial_pose_is_over_a_free_cell() {
        let grid = Grid::from_text(DEFAULT_MAP).unwrap();
        let (gx, gy) = grid.world_to_grid(DEFAULT_INITIAL_POSE.x, DEFAULT_INITIAL_POSE.y);
        let cell = grid.cell(gx.floor() as i64, gy.floor() as i64);
        assert_eq!(
            cell,
            Some(false),
            "the default initial pose must not start inside a wall"
        );
    }

    #[test]
    fn the_compile_time_geometry_and_lidar_constants_are_valid() {
        assert!(DiffDriveGeometry::new(WHEEL_BASE, WHEEL_RADIUS).is_some());
        assert!(LidarConfig::full_circle(LIDAR_RAYS, LIDAR_RANGE_MIN, LIDAR_RANGE_MAX).is_some());
        assert!(Timestep::from_hz(TICK_HZ).is_some());
    }

    /// `build_world`'s lidar must actually use `LidarConfig::full_circle`
    /// (evenly spaced bearings around the whole circle, no duplicated seam
    /// beam) — the regression this crate's own development caught, per
    /// `LidarConfig::new`'s own docs.
    #[test]
    fn the_default_lidar_has_no_duplicated_seam_beam() {
        for var in [ENV_SEED, ENV_NOISE_SIGMA, ENV_TICKS, ENV_MAP] {
            // SAFETY (concurrency, not memory): see
            // `build_world_produces_a_fresh_world_at_the_default_pose`'s
            // own comment on why this is sound for this crate's suite.
            unsafe { std::env::remove_var(var) };
        }
        let (world, ..) = build_world().unwrap();
        let expected_increment = std::f64::consts::TAU / f64::from(LIDAR_RAYS);
        assert!(
            (world.lidar_config().angle_increment() - expected_increment).abs() < 1e-9,
            "increment was {}, expected {expected_increment}",
            world.lidar_config().angle_increment()
        );
    }

    #[test]
    fn build_world_produces_a_fresh_world_at_the_default_pose() {
        // SAFETY (concurrency, not memory): `std::env::set_var`/`remove_var`
        // mutate process-global state; this test only reads/writes env vars
        // this binary itself defines, and nextest runs each test in its own
        // process by default, so no cross-test interference is possible in
        // practice for this crate's own suite.
        for var in [ENV_SEED, ENV_NOISE_SIGMA, ENV_TICKS, ENV_MAP] {
            unsafe { std::env::remove_var(var) };
        }
        let (world, seed, sigma) = build_world().unwrap();
        assert_eq!(world.tick_count(), 0);
        assert_eq!(world.ground_truth_pose(), DEFAULT_INITIAL_POSE);
        assert_eq!(seed, DEFAULT_SEED);
        assert_eq!(sigma, DEFAULT_NOISE_SIGMA);
    }

    #[test]
    fn the_port_names_match_the_manifest() {
        assert_eq!(TICK_PORT, "tick");
        assert_eq!(CMD_VEL_PORT, "cmd_vel");
        assert_eq!(ODOM_PORT, "odom");
        assert_eq!(SCAN_PORT, "scan");
        assert_eq!(TF_PORT, "tf");
    }
}
