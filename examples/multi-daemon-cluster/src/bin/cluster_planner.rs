//! `cluster-planner` — the far half of the graph, on machine `robot-b`
//! (blueprint §4.2, §6.4).
//!
//! Answers every [`Reading`] with a [`Command`] carrying **two** placement
//! claims: where this planner is running, and where the reading it answers
//! came from. The checker back on `robot-a` compares them, which is how a
//! two-hop crossing becomes an assertion rather than an assumption.
//!
//! ```text
//!   (sensor @ robot-a) ──readings──► [planner @ robot-b] ──commands──► (checker @ robot-a)
//! ```
//!
//! Both of its edges cross the machine boundary, in opposite directions — the
//! shortest graph that exercises a peer route each way (§6.4).

use std::process::ExitCode;

use astrs_node_api::{Event, Node};
use multi_daemon_cluster::{COMMANDS_PORT, Command, READINGS_PORT, Reading, machine_of};

fn main() -> ExitCode {
    match plan() {
        Ok(commands) => {
            println!("cluster-planner: answered {commands} readings");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("cluster-planner: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Turns every reading into one command.
fn plan() -> Result<u64, Box<dyn std::error::Error>> {
    let (mut node, mut events) = Node::init_from_env()?;
    let machine = machine_of(&node);
    let mut commands = node.raw_output(COMMANDS_PORT)?;
    node.log_info(format!("planner up on {machine}"));

    let mut answered = 0_u64;
    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, meta, data } if id.as_str() == READINGS_PORT => {
                let reading = Reading::from_bytes(&data.to_vec())?;
                if answered == 0 {
                    node.log_info(format!(
                        "first reading came from {} (this planner is on {machine})",
                        reading.origin
                    ));
                }
                let command = Command {
                    seq: reading.seq,
                    planner: machine.clone(),
                    upstream: reading.origin,
                };
                commands.send_bytes(command.to_bytes()?, meta.follow())?;
                answered += 1;
            }
            // A peer partition surfaces as an ordinary closed input (§12), so
            // a planner that loses its sensor finishes rather than hanging.
            Event::InputClosed { id, reason, .. } => {
                node.log_info(format!("{id} closed: {reason}"));
                break;
            }
            Event::AllInputsClosed => break,
            Event::Stop(cause) => {
                node.log_info(format!("stopping after {answered} commands: {cause}"));
                break;
            }
            _ => {}
        }
    }

    // The control lane pre-empts queued data (§11.2), so the notice that ended
    // the loop can have overtaken readings still queued behind it.
    while let Some(event) = events.try_recv() {
        if let Event::Input { id, meta, data } = event
            && id.as_str() == READINGS_PORT
        {
            let reading = Reading::from_bytes(&data.to_vec())?;
            let command = Command {
                seq: reading.seq,
                planner: machine.clone(),
                upstream: reading.origin,
            };
            commands.send_bytes(command.to_bytes()?, meta.follow())?;
            answered += 1;
        }
    }

    commands.close()?;
    node.log_info(format!("answered {answered} readings from {machine}"));
    Ok(answered)
}
