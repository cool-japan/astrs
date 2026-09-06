//! The AstRS replay node binary.
//!
//! See [`astrs_replay_node`] for the replay loop itself; this binary is
//! a thin CLI shell over it.

use clap::Parser;

fn main() {
    let args = astrs_replay_node::ReplayArgs::parse();
    if let Err(error) = astrs_replay_node::run(&args) {
        eprintln!("astrs-replay-node: {error}");
        std::process::exit(1);
    }
}
