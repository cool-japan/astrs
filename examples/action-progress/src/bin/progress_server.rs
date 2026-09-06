//! `progress-server` — the answering half of `action-progress` (blueprint
//! §9.4).
//!
//! Executes every goal in four steps: three `Executing` feedback updates
//! carrying increasing fractional progress, then one terminal update —
//! `Succeeded` for a non-negative target, `Aborted` for a negative one (see
//! the crate docs for why one target is always negative).
//!
//! ```text
//!   [client] ──goal──► [server] ──status──► [client]
//! ```
//!
//! # A goal is not a status update
//!
//! [`astrs_node_api::ActionOutcome::from_event`] deliberately does not
//! match the incoming `goal` message: it carries `goal_id` but no
//! `goal_status`, which is what tells the two apart (§9.4). This node reads
//! the goal directly off the event instead.

use std::process::ExitCode;

use action_progress::{
    ABORTED_FRACTION, FEEDBACK_FRACTIONS, GOAL_PORT, SUCCEEDED_FRACTION, expected_status_for,
};
use astrs_node_api::message::Scalar;
use astrs_node_api::{Event, GoalId, Node, Output};
use astrs_wire::GoalStatus;

fn main() -> ExitCode {
    match serve() {
        Ok(served) => {
            println!("progress-server: executed {served} goals");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("progress-server: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Executes every goal that arrives until the client's port closes.
fn serve() -> Result<u64, Box<dyn std::error::Error>> {
    let (mut node, mut events) = Node::init_from_env()?;
    let mut status = node.output::<Scalar<f64>>("status")?;
    node.log_info("server up");

    let mut served = 0_u64;
    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, meta, data } if id.as_str() == GOAL_PORT => {
                let Some(goal_id) = meta.goal_id().map(GoalId::new) else {
                    node.log_warn("a message on the goal port carried no goal_id");
                    continue;
                };
                let Scalar(target) = data.view::<Scalar<i64>>()?;
                execute(&node, &mut status, &goal_id, target)?;
                served += 1;
            }
            Event::InputClosed { ref id, .. } if id.as_str() == GOAL_PORT => {
                node.log_info("the client is done; finishing");
                break;
            }
            Event::Stop(cause) => {
                node.log_info(format!("stopping after {served} goals: {cause}"));
                break;
            }
            _ => {}
        }
    }

    status.close()?;
    node.log_info(format!("executed {served} goals"));
    Ok(served)
}

/// Runs one goal to completion: feedback, then the terminal status.
fn execute(
    node: &Node,
    status: &mut Output<Scalar<f64>>,
    goal_id: &GoalId,
    target: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    node.log_info(format!("goal {goal_id}: target {target}"));
    for fraction in FEEDBACK_FRACTIONS {
        node.goal_status(status, goal_id, GoalStatus::Executing, *fraction)?;
    }
    let (final_status, fraction) = match expected_status_for(target) {
        GoalStatus::Aborted => (GoalStatus::Aborted, ABORTED_FRACTION),
        _ => (GoalStatus::Succeeded, SUCCEEDED_FRACTION),
    };
    node.goal_status(status, goal_id, final_status, fraction)?;
    Ok(())
}
