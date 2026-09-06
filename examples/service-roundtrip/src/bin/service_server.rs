//! `service-server` — the answering half of the §9.4 service pattern.
//!
//! Squares every number it is asked about, and answers on the response port
//! with the request's own correlation id.
//!
//! ```text
//!   [client] ──request──► [server] ──response──► [client]
//! ```
//!
//! # A server is an ordinary node
//!
//! No RPC runtime, no dispatch table, no generated stubs: the request arrives
//! as an [`Event::Input`], [`ServiceRequest::from_event`] confirms it carries
//! a correlation id, and [`Node::service_response`] copies that id onto the
//! reply. A message on the same port *without* an id is not a service call
//! and is refused rather than answered into the void.

use std::process::ExitCode;

use astrs_node_api::message::Scalar;
use astrs_node_api::{Event, Node, ServiceRequest};
use service_roundtrip::{REQUEST_PORT, RESPONSE_PORT, expected_answer};

fn main() -> ExitCode {
    match serve() {
        Ok(served) => {
            println!("service-server: answered {served} requests");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("service-server: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Answers requests until the client's port closes.
fn serve() -> Result<u64, Box<dyn std::error::Error>> {
    let (mut node, mut events) = Node::init_from_env()?;
    let mut responses = node.output::<Scalar<i64>>(RESPONSE_PORT)?;
    node.log_info("server up");

    let mut served = 0_u64;
    while let Some(event) = events.recv() {
        match event {
            Event::Input { ref id, .. } if id.as_str() == REQUEST_PORT => {
                let Ok(request) = ServiceRequest::from_event(event) else {
                    node.log_warn("a message on the request port carried no request_id");
                    continue;
                };
                let Scalar(value) = request.view::<Scalar<i64>>()?;
                // The request's metadata, not a fresh one: that is what
                // carries the id back to the caller (§9.4).
                node.service_response(
                    &mut responses,
                    &request.metadata,
                    Scalar(expected_answer(value)),
                )?;
                served += 1;
            }
            Event::InputClosed { ref id, .. } if id.as_str() == REQUEST_PORT => {
                node.log_info("the client is done; finishing");
                break;
            }
            Event::Stop(cause) => {
                node.log_info(format!("stopping after {served} answers: {cause}"));
                break;
            }
            _ => {}
        }
    }

    responses.close()?;
    node.log_info(format!("answered {served} requests"));
    Ok(served)
}
