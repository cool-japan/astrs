//! The AstRS ROS 2 bridge node binary.
//!
//! See [`astrs_ros2_bridge_node`] for the bridge itself; this binary is a
//! thin shell over it that turns a typed [`BridgeError`] into a message and
//! an exit code (`78`, `sysexits.h`'s `EX_CONFIG`, for anything a manifest
//! author can fix; `1` for a lost daemon), so a supervisor can tell a bad
//! manifest from a crashed cluster without parsing text.
//!
//! The bridge takes no arguments at all: everything it needs comes from the
//! spawn handshake (`ASTRS_NODE_CONFIG`) and the node's `env:` block.

use astrs_ros2_bridge_node::BridgeError;

fn main() {
    if let Err(error) = astrs_ros2_bridge_node::run() {
        report(&error);
        std::process::exit(error.exit_code());
    }
}

/// Print `error` to stderr, with the "this is a configuration fault" hint
/// when it is one.
fn report(error: &BridgeError) {
    eprintln!("astrs-ros2-bridge-node: {error}");
    if error.is_startup() {
        eprintln!(
            "astrs-ros2-bridge-node: nothing was bridged; fix the node's \
             `ros2:` block and restart the dataflow"
        );
    }
}
