//! Library-testable command implementations, one module per verb group.
//!
//! Every function here takes a `&mut dyn Write` output sink and a typed
//! args struct, and returns a typed report (or `Result<Report, CliError>`
//! when the command can fail outright) — no `println!`, no
//! `process::exit`, so every verb is testable by constructing args by
//! hand and inspecting the returned report and the bytes written to an
//! in-memory `Vec<u8>` sink.

pub mod bag;
pub mod build;
pub mod client;
pub mod cluster;
pub mod completion;
pub mod doctor;
pub mod expand;
pub mod graph;
pub mod hub;
pub mod lifecycle;
pub mod log_stream;
pub mod migrate;
pub mod monitor;
pub mod new;
pub mod node;
pub mod param;
pub mod record;
pub mod replay;
pub mod ros2;
pub mod ros2_doctor;
pub mod ros2_topics;
pub mod run;
pub mod schema;
pub mod serve;
pub mod signals;
pub mod stub;
pub mod token;
pub mod top;
pub mod topic;
pub mod trace;
pub mod validate;
