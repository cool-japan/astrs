//! Starting node processes — environment hygiene, argv, generations (§16).
//!
//! | Module | Concern |
//! |---|---|
//! | [`mod@env`] | The three-filter environment pipeline: inherited allowlist, manifest denylist, daemon-owned last |
//! | [`argv`] | Splitting a command line by shell *rules*, never through a shell |
//! | [`blob`] | The `ASTRS_NODE_CONFIG` / `ASTRS_RUN_PARENT_PID` variables the daemon owns |
//! | [`handle`] | The generation-stamped signalling handle, and why every kill is guarded |
//! | [`affinity`] | Applying `cpu_affinity` at spawn — race-free on Linux, honestly unsupported elsewhere (§11.3) |
//! | [`rt`] | Applying `rt` (hard-RT executor reservations) at spawn — race-free `SCHED_FIFO`/`SCHED_RR` on Linux x86_64/aarch64, `EPERM` is a spawn failure, honestly unsupported elsewhere (§11.3, §22) |
//! | [`process`] | The assembly point: spec in, running child out |
//!
//! # Generations
//!
//! Every incarnation of a node gets a number (§12). The number is not a
//! bookkeeping detail — it is stamped into three places at spawn time, and
//! each one closes a different stale-state hole:
//!
//! 1. **The handshake blob** ([`astrs_wire::NodeConfig::generation`]), so the
//!    node presents it in `Register` and the daemon can refuse a zombie from a
//!    previous incarnation that is still trying to connect.
//! 2. **The shared-memory segment key**
//!    ([`astrs_shm::SegmentKey::with_generation`], via
//!    [`segment_key_for`]), so a consumer still mapping the old segment reads
//!    a stale stamp and knows to re-attach rather than reading a recycled
//!    slot.
//! 3. **The signalling handle** ([`handle::ProcessHandle`]), so a timer armed
//!    against generation *n* cannot kill generation *n+1*.
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::spawn::{next_generation, segment_key_for};
//! use astrs_wire::{DataflowId, DataId, NodeId};
//!
//! assert_eq!(next_generation(0), 1);
//!
//! let first = segment_key_for(
//!     DataflowId::from_u128(1), &NodeId::new("camera")?, &DataId::new("image")?, 1,
//! );
//! let second = segment_key_for(
//!     DataflowId::from_u128(1), &NodeId::new("camera")?, &DataId::new("image")?, 2,
//! );
//! assert_ne!(first.canonical(), second.canonical(), "a restart is a new segment");
//! # Ok::<(), astrs_wire::IdError>(())
//! ```

pub mod affinity;
pub mod argv;
pub mod blob;
pub mod env;
pub mod handle;
pub mod process;
pub mod rt;

pub use affinity::CpuAffinityOutcome;
pub use argv::{ArgvError, CommandLine, split, split_non_empty};
pub use blob::DaemonOwnedVars;
pub use env::{
    BuiltEnv, DENIED_NAMES, DENIED_PREFIXES, DeniedVar, DenyReason, EnvError, EnvPolicy,
    deny_reason, process_env,
};
pub use handle::{ProcessHandle, SignalOutcome};
pub use process::{SpawnRequest, SpawnedProcess, Spawner, StdioMode, command_for};
pub use rt::RtOutcome;

use astrs_shm::SegmentKey;
use astrs_wire::{DataId, DataflowId, NodeId};

/// The generation a restart moves to.
///
/// Saturating rather than wrapping: a node that has restarted `u64::MAX` times
/// has bigger problems than a counter, and wrapping would make a stale handle
/// look current.
#[must_use]
pub const fn next_generation(current: u64) -> u64 {
    current.saturating_add(1)
}

/// The shared-memory segment key for one output of one incarnation.
///
/// The generation is part of the key, so a restarted producer never publishes
/// into the segment its previous incarnation's consumers are still mapping
/// (§6.2, §12).
#[must_use]
pub fn segment_key_for(
    dataflow: DataflowId,
    node: &NodeId,
    output: &DataId,
    generation: u64,
) -> SegmentKey {
    SegmentKey::new(dataflow, node.clone(), output.clone(), generation)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn generations_count_up_and_saturate() {
        assert_eq!(next_generation(0), 1);
        assert_eq!(next_generation(41), 42);
        assert_eq!(next_generation(u64::MAX), u64::MAX);
    }

    #[test]
    fn a_segment_key_changes_with_every_incarnation() {
        let dataflow = DataflowId::from_u128(7);
        let node = NodeId::new("camera").unwrap();
        let output = DataId::new("image").unwrap();

        let keys: Vec<String> = (0..4)
            .map(|generation| segment_key_for(dataflow, &node, &output, generation).canonical())
            .collect();
        let mut unique = keys.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), keys.len(), "generations collide: {keys:?}");
    }

    #[test]
    fn a_segment_key_names_its_producer_and_output() {
        let key = segment_key_for(
            DataflowId::from_u128(7),
            &NodeId::new("camera").unwrap(),
            &DataId::new("image").unwrap(),
            2,
        );
        let canonical = key.canonical();
        assert!(canonical.contains("camera"), "{canonical}");
        assert!(canonical.contains("image"), "{canonical}");
    }

    #[test]
    fn two_dataflows_never_share_a_segment() {
        let node = NodeId::new("camera").unwrap();
        let output = DataId::new("image").unwrap();
        assert_ne!(
            segment_key_for(DataflowId::from_u128(1), &node, &output, 0).canonical(),
            segment_key_for(DataflowId::from_u128(2), &node, &output, 0).canonical()
        );
    }
}
