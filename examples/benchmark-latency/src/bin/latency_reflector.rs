//! `latency-reflector` — the echo half of `benchmark-latency` (blueprint
//! §20.4).
//!
//! Echoes every `ping` payload back on `pong`, unchanged, as fast as it
//! arrives. No state, no correlation bookkeeping: `latency-prober` is the
//! side that times anything, and it can because it never has more than one
//! round trip outstanding (see that binary's docs).

use std::process::ExitCode;

use astrs_node_api::{Event, Node};
use benchmark_latency::PING_PORT;

fn main() -> ExitCode {
    match reflect() {
        Ok(echoed) => {
            println!("latency-reflector: echoed {echoed} pings");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("latency-reflector: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Echoes every `ping` until the prober's port closes.
fn reflect() -> Result<u64, Box<dyn std::error::Error>> {
    let (mut node, mut events) = Node::init_from_env()?;
    let mut pong = node.raw_output("pong")?;
    node.log_info("reflector up");

    let mut echoed = 0_u64;
    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, data, .. } if id.as_str() == PING_PORT => {
                let metadata = node.metadata();
                pong.send_bytes(data.to_vec(), metadata)?;
                echoed += 1;
            }
            Event::InputClosed { id, .. } if id.as_str() == PING_PORT => {
                node.log_info("the prober is done; finishing");
                break;
            }
            Event::Stop(cause) => {
                node.log_info(format!("stopping after {echoed} echoes: {cause}"));
                break;
            }
            _ => {}
        }
    }

    pong.close()?;
    node.log_info(format!("echoed {echoed} pings"));
    Ok(echoed)
}
