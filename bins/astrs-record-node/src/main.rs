//! The AstRS recorder node binary.
//!
//! See [`astrs_record_node`] for the recording loop itself; this binary
//! is a thin CLI shell over it.

use clap::Parser;

fn main() {
    let args = astrs_record_node::RecordArgs::parse();
    if let Err(error) = astrs_record_node::run(&args) {
        eprintln!("astrs-record-node: {error}");
        std::process::exit(1);
    }
}
