//! `service-client` — the calling half of the §9.4 service pattern.
//!
//! Issues one request per timer tick, matches every response to the request
//! it answers, checks the answer, and writes a JSON result when the budget is
//! spent.
//!
//! ```text
//!   astrs/timer/millis/50 ──► tick ──► [client] ──request──► [server]
//!                                          ▲                     │
//!                                          └─────response────────┘
//! ```
//!
//! # Correlation is the whole protocol
//!
//! [`Node::service_request`] stamps a fresh [`RequestId`] into the outgoing
//! metadata and hands it back. [`ServiceResponse::from_event`] reads that same
//! id out of the reply. A client with several calls in flight therefore needs
//! no sequence discipline of its own — which is the point of doing it in
//! metadata rather than in the payload.

use std::collections::BTreeMap;
use std::process::ExitCode;
use std::time::Instant;

use astrs_node_api::message::Scalar;
use astrs_node_api::{Event, Node, RequestId, ServiceResponse};
use service_roundtrip::{
    REQUEST_PORT, RESPONSE_PORT, RoundTripResult, TICK_PORT, call_budget, expected_answer,
    result_path,
};

fn main() -> ExitCode {
    let path = result_path();
    match call() {
        Ok(result) => {
            println!(
                "service-client: {}/{} answered, {} correct, max {} us",
                result.responses, result.requests, result.correct, result.max_latency_us
            );
            if let Err(error) = write_result(&path, &result) {
                eprintln!("service-client: could not write the result: {error}");
                return ExitCode::FAILURE;
            }
            if result.is_clean() {
                ExitCode::SUCCESS
            } else {
                eprintln!("service-client: the round trip was not clean: {result:?}");
                ExitCode::FAILURE
            }
        }
        Err(error) => {
            eprintln!("service-client: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Writes the result where the manifest asked for it.
fn write_result(
    path: &std::path::Path,
    result: &RoundTripResult,
) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::write(path, result.to_json()?)?;
    Ok(())
}

/// Issues the call budget and collects every answer.
fn call() -> Result<RoundTripResult, Box<dyn std::error::Error>> {
    let budget = call_budget();
    let (mut node, mut events) = Node::init_from_env()?;
    let mut requests = node.output::<Scalar<i64>>(REQUEST_PORT)?;
    node.log_info(format!("client up: {budget} calls to make"));

    // What was asked, and when — the client's whole state.
    let mut outstanding: BTreeMap<RequestId, (i64, Instant)> = BTreeMap::new();
    let mut result = RoundTripResult {
        requests: 0,
        responses: 0,
        correct: 0,
        uncorrelated: 0,
        max_latency_us: 0,
    };

    while let Some(event) = events.recv() {
        match event {
            Event::Input { ref id, .. } if id.as_str() == TICK_PORT && result.requests < budget => {
                let value = i64::try_from(result.requests).unwrap_or(0) + 1;
                let id = node.service_request(&mut requests, Scalar(value))?;
                outstanding.insert(id, (value, Instant::now()));
                result.requests += 1;
            }
            Event::Input { ref id, .. } if id.as_str() == RESPONSE_PORT => {
                // `from_event` gives back the event when it is not a
                // correlated reply, so an ordinary message on the same port
                // is not silently treated as an answer.
                let Ok(response) = ServiceResponse::from_event(event) else {
                    result.uncorrelated += 1;
                    continue;
                };
                match outstanding.remove(&response.id) {
                    Some((asked, sent)) => {
                        result.responses += 1;
                        let latency = u64::try_from(sent.elapsed().as_micros()).unwrap_or(u64::MAX);
                        result.max_latency_us = result.max_latency_us.max(latency);
                        let Scalar(answer) = response.view::<Scalar<i64>>()?;
                        if answer == expected_answer(asked) {
                            result.correct += 1;
                        } else {
                            node.log_warn(format!(
                                "{asked} squared should be {}, the server said {answer}",
                                expected_answer(asked)
                            ));
                        }
                    }
                    None => result.uncorrelated += 1,
                }
                if result.requests >= budget && outstanding.is_empty() {
                    break;
                }
            }
            Event::Stop(cause) => {
                node.log_info(format!(
                    "stopping with {} answered: {cause}",
                    result.responses
                ));
                break;
            }
            _ => {}
        }
    }

    requests.close()?;
    node.log_info(format!(
        "{} of {} calls answered correctly",
        result.correct, result.requests
    ));
    Ok(result)
}
