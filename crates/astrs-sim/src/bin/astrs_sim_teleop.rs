//! `astrs-sim-teleop` — a deterministic `cmd_vel` source: drives straight,
//! then curves, then stops (blueprint §5.4-style worked example, this
//! crate's own version).
//!
//! ```text
//!   astrs/timer/millis/N ──► tick ──► [astrs-sim-teleop] ──cmd_vel──►
//! ```
//!
//! A real teleop node relays a human's joystick; this one relays a fixed,
//! deterministic schedule instead — [`command_for_tick`] — so that running
//! `dataflow.yml` twice produces the exact same commanded path both times,
//! which is the whole point of pairing it with `astrs-sim-node`'s own
//! determinism story. The schedule drives straight for the first half of
//! its tick budget, then arcs for the second half, so a run exercises both
//! branches of `astrs_sim::kinematics::integrate_unicycle` (see that
//! function's own docs), not only a straight line.

use std::process::ExitCode;

use astrs_node_api::{Event, Node};
use astrs_sim::kinematics::Velocity2D;
use astrs_sim::wire::velocity_to_twist;

/// The timer input, as named in `dataflow.yml`.
const TICK_PORT: &str = "tick";
/// The commanded-velocity output.
const CMD_VEL_PORT: &str = "cmd_vel";

/// Overrides how many ticks this node runs for before finishing.
const ENV_TICKS: &str = "ASTRS_SIM_TELEOP_TICKS";
/// The default tick budget — matches `astrs-sim-node`'s own default so
/// the two finish together in `dataflow.yml`'s stock configuration.
const DEFAULT_TICKS: u64 = 400;

/// Forward speed during both phases, in metres/second.
const DRIVE_LINEAR: f64 = 0.4;
/// Turn rate during the arc phase, in radians/second.
const TURN_ANGULAR: f64 = 0.6;

fn main() -> ExitCode {
    match run() {
        Ok(ticks) => {
            println!("astrs-sim-teleop: sent {ticks} commands");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("astrs-sim-teleop: {error}");
            ExitCode::FAILURE
        }
    }
}

/// How many ticks this run should send commands for.
fn tick_budget(raw: Option<&str>) -> u64 {
    raw.and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|ticks| *ticks > 0)
        .unwrap_or(DEFAULT_TICKS)
}

/// The command this schedule sends at `tick` (0-indexed) out of a total
/// `budget`: straight for the first half, an arc for the second half.
///
/// A `budget` of `0` never reaches either branch in practice (the caller's
/// loop would not run), but is handled the same way `1` is (whichever
/// half `tick` falls in) rather than dividing by zero — `budget / 2`
/// saturates to `0` for `budget <= 1`, making every tick fall into the
/// "at or past the midpoint" arc branch.
#[must_use]
fn command_for_tick(tick: u64, budget: u64) -> Velocity2D {
    if tick < budget / 2 {
        Velocity2D::new(DRIVE_LINEAR, 0.0)
    } else {
        Velocity2D::new(DRIVE_LINEAR, TURN_ANGULAR)
    }
}

/// Sends the schedule to completion, returning how many commands were
/// sent.
fn run() -> Result<u64, Box<dyn std::error::Error>> {
    let budget = tick_budget(std::env::var(ENV_TICKS).ok().as_deref());
    let (mut node, mut events) = Node::init_from_env()?;
    let mut cmd_vel_out = node.output::<astrs_node_api::message::Twist>(CMD_VEL_PORT)?;
    node.log_info(format!("astrs-sim-teleop up: {budget} commands"));

    let mut ticks = 0_u64;
    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, meta, .. } if id.as_str() == TICK_PORT => {
                let velocity = command_for_tick(ticks, budget);
                cmd_vel_out.send(velocity_to_twist(velocity), meta.follow())?;
                ticks += 1;
                if ticks >= budget {
                    break;
                }
            }
            Event::Stop(cause) => {
                node.log_info(format!("stopping after {ticks} commands: {cause}"));
                break;
            }
            _ => {}
        }
    }

    cmd_vel_out.close()?;
    node.log_info(format!("sent {ticks} commands"));
    Ok(ticks)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn tick_budget_falls_back_to_the_default_for_unusable_input() {
        assert_eq!(tick_budget(Some("100")), 100);
        for raw in [None, Some(""), Some("lots"), Some("0"), Some("-1")] {
            assert_eq!(tick_budget(raw), DEFAULT_TICKS, "{raw:?}");
        }
    }

    #[test]
    fn the_first_half_of_the_schedule_drives_straight() {
        for tick in [0, 1, 49] {
            let cmd = command_for_tick(tick, 100);
            assert_eq!(cmd, Velocity2D::new(DRIVE_LINEAR, 0.0), "tick={tick}");
        }
    }

    #[test]
    fn the_second_half_of_the_schedule_arcs() {
        for tick in [50, 51, 99] {
            let cmd = command_for_tick(tick, 100);
            assert_eq!(
                cmd,
                Velocity2D::new(DRIVE_LINEAR, TURN_ANGULAR),
                "tick={tick}"
            );
        }
    }

    #[test]
    fn the_schedule_is_a_pure_function_of_tick_and_budget() {
        assert_eq!(command_for_tick(10, 100), command_for_tick(10, 100));
        assert_ne!(command_for_tick(10, 100), command_for_tick(90, 100));
    }

    #[test]
    fn a_degenerate_budget_never_divides_by_zero() {
        // Every tick falls into the arc branch when `budget <= 1`, rather
        // than panicking on `budget / 2`.
        assert_eq!(
            command_for_tick(0, 0),
            Velocity2D::new(DRIVE_LINEAR, TURN_ANGULAR)
        );
        assert_eq!(
            command_for_tick(0, 1),
            Velocity2D::new(DRIVE_LINEAR, TURN_ANGULAR)
        );
    }

    #[test]
    fn the_port_names_and_default_ticks_match_the_manifest() {
        assert_eq!(TICK_PORT, "tick");
        assert_eq!(CMD_VEL_PORT, "cmd_vel");
        assert_eq!(ENV_TICKS, "ASTRS_SIM_TELEOP_TICKS");
        assert_eq!(DEFAULT_TICKS, 400);
    }
}
