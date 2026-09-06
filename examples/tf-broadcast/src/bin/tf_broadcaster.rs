//! `tf-broadcaster` — publishes a latched static frame and a sliding
//! dynamic one (blueprint §10.6).
//!
//! ```text
//!   astrs/timer/millis/20 ──► tick ──► [tf-broadcaster] ──map_odom (tick 0 only)──►
//!                                                       └─odom_base (every tick)──►
//! ```
//!
//! `map_odom` is sent exactly once, on the first tick — the `/tf_static`
//! convention: a value valid at any query time needs no repeating.
//! `odom_base` is sent on every tick, sliding `base_link` along X — the
//! `/tf` convention: a value that keeps changing.

use std::process::ExitCode;

use astrs_node_api::{Event, Node};
use tf_broadcast::{
    ENV_TICKS, MAP_ODOM_PORT, ODOM_BASE_PORT, TICK_PORT, map_odom_transform, odom_base_transform,
    tick_budget,
};

fn main() -> ExitCode {
    match broadcast() {
        Ok(ticks) => {
            println!("tf-broadcaster: broadcast {ticks} ticks");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("tf-broadcaster: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Broadcasts the static offset once and the dynamic frame every tick,
/// until the budget runs out.
fn broadcast() -> Result<u64, Box<dyn std::error::Error>> {
    let budget = tick_budget(std::env::var(ENV_TICKS).ok().as_deref());
    let (mut node, mut events) = Node::init_from_env()?;
    let mut map_odom = node.output::<astrs_node_api::message::Transform>(MAP_ODOM_PORT)?;
    let mut odom_base = node.output::<astrs_node_api::message::Transform>(ODOM_BASE_PORT)?;
    node.log_info(format!("tf-broadcaster up: {budget} ticks"));

    let mut ticks = 0_u64;
    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, meta, .. } if id.as_str() == TICK_PORT => {
                if ticks == 0 {
                    // Latched: sent once, valid at any query time.
                    map_odom.send(map_odom_transform(), meta.follow())?;
                }
                odom_base.send(odom_base_transform(ticks), meta.follow())?;
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

    // Closing tells `tf-consumer` that no more frames are coming, so it
    // finishes its own run instead of waiting.
    map_odom.close()?;
    odom_base.close()?;
    node.log_info(format!("broadcast {ticks} ticks"));
    Ok(ticks)
}
