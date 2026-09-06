//! `progress-client` — the calling half of `action-progress` (blueprint
//! §9.4).
//!
//! Submits [`GOAL_TARGETS`] one at a time, walking each through
//! [`GoalTracker`]'s enforced FSM and recording every `Executing` feedback
//! fraction until a terminal status arrives.
//!
//! ```text
//!   [client] ──goal──► [server] ──status──► [client]
//! ```

use std::process::ExitCode;

use action_progress::{ActionRunReport, GOAL_TARGETS, GoalRun, STATUS_PORT, report_path};
use astrs_node_api::message::Scalar;
use astrs_node_api::{ActionOutcome, Event, EventStream, GoalTracker, Node, Output};

fn main() -> ExitCode {
    match run() {
        Ok(report) => {
            let expected = GOAL_TARGETS.len();
            println!(
                "progress-client: {}/{expected} goals ran, all_correct={}",
                report.runs.len(),
                report.all_correct(expected)
            );
            if report.all_correct(expected) {
                ExitCode::SUCCESS
            } else {
                eprintln!("progress-client: not every goal was correct: {report:?}");
                ExitCode::FAILURE
            }
        }
        Err(error) => {
            eprintln!("progress-client: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Submits every configured goal, one at a time, and writes the report.
fn run() -> Result<ActionRunReport, Box<dyn std::error::Error>> {
    let (mut node, mut events) = Node::init_from_env()?;
    let mut goal_out = node.output::<Scalar<i64>>("goal")?;
    node.log_info(format!("client up: {} goals to submit", GOAL_TARGETS.len()));

    let mut runs = Vec::new();
    for target in GOAL_TARGETS {
        match run_one_goal(&node, &mut events, &mut goal_out, *target)? {
            Some(run) => runs.push(run),
            None => break, // the session ended before this goal finished
        }
    }

    goal_out.close()?;
    let report = ActionRunReport::new(runs);
    let path = report_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, report.to_json()?)?;
    node.log_info(format!(
        "wrote {} goal runs to {}",
        report.runs.len(),
        path.display()
    ));
    Ok(report)
}

/// Submits one goal and drives it to a terminal status, or `None` if the
/// session ended first.
fn run_one_goal(
    node: &Node,
    events: &mut EventStream,
    goal_out: &mut Output<Scalar<i64>>,
    target: i64,
) -> Result<Option<GoalRun>, Box<dyn std::error::Error>> {
    let goal_id = node.goal(goal_out, target)?;
    let mut tracker = GoalTracker::new();
    tracker.track(goal_id.clone());
    let mut run = GoalRun::new(target, goal_id.as_str());

    while let Some(event) = events.recv() {
        match event {
            Event::Input { ref id, .. } if id.as_str() == STATUS_PORT => {
                let Ok(outcome) = ActionOutcome::from_event(event) else {
                    continue; // not a correlated status update
                };
                if outcome.goal != goal_id {
                    continue; // a stray update for a goal this run is not tracking
                }
                if outcome.is_feedback() {
                    let Scalar(fraction) = outcome.view::<Scalar<f64>>()?;
                    run.feedback.push(fraction);
                }
                let status = tracker.observe(&outcome)?;
                if status.is_terminal() {
                    if let Ok(Scalar(fraction)) = outcome.view::<Scalar<f64>>() {
                        run.final_fraction = Some(fraction);
                    }
                    run.final_status = Some(status);
                    return Ok(Some(run));
                }
            }
            Event::Stop(cause) => {
                node.log_info(format!("stopping mid-goal {goal_id}: {cause}"));
                return Ok(None);
            }
            _ => {}
        }
    }
    Ok(None)
}
