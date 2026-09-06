//! `typed-any-source` — publishes the same deterministic sequence twice:
//! once typed, once raw (blueprint §9.1, §9.2).
//!
//! ```text
//!   astrs/timer/millis/10 ──► tick ──► [source] ──typed──► (sink)
//!                                          └──────raw─────► (sink)
//! ```

use std::process::ExitCode;

use astrs_node_api::{Event, Node};
use typed_vs_any::{
    RAW_PORT, TICK_PORT, TYPED_PORT, TypedValue, raw_payload, sample_budget, value_for,
};

fn main() -> ExitCode {
    match publish() {
        Ok(published) => {
            println!("typed-any-source: published {published} values on both ports");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("typed-any-source: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Publishes the sample budget on both ports, then closes them.
fn publish() -> Result<u64, Box<dyn std::error::Error>> {
    let budget = sample_budget();
    let (mut node, mut events) = Node::init_from_env()?;
    let mut typed = node.output::<TypedValue>(TYPED_PORT)?;
    let mut raw = node.raw_output(RAW_PORT)?;
    node.log_info(format!("source up: {budget} values on `typed` and `raw`"));

    let mut published = 0_u64;
    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, meta, .. } if id.as_str() == TICK_PORT => {
                let value = value_for(published);
                // `meta.follow()` on both sends: each pair is stamped as
                // caused by the same tick (§4.3), which is what makes them
                // one logical sample rather than two coincidentally equal
                // ones.
                typed.send(value, meta.follow())?;
                raw.send_bytes(raw_payload(value), meta.follow())?;
                published += 1;
                if published >= budget {
                    break;
                }
            }
            Event::Stop(cause) => {
                node.log_info(format!("stopping after {published} values: {cause}"));
                break;
            }
            _ => {}
        }
    }

    typed.close()?;
    raw.close()?;
    node.log_info(format!("published {published} values on both ports"));
    Ok(published)
}
