//! [`RegistryCommand`]: what a coordinator replicates.
//!
//! # The authoritative mutable state, and why it is already named
//!
//! A coordinator's durable truth lives in `astrs-store`: the dataflow
//! registry, per-node status, the daemon registry, parameters and the build
//! cache. Every mutating call on that store already emits exactly one
//! [`MutationOp`] describing the change — and each `*Put` variant carries the
//! **complete new record**, not a delta, precisely so that replaying it is an
//! unconditional overwrite ([`astrs_store::CoordinatorStore::apply_replayed`]).
//!
//! That makes `MutationOp` the natural Raft command. It is already
//! `oxicode`-encoded, already append-only-versioned, and already replayable
//! with no knowledge of prior state — the three properties a replicated
//! state-machine command must have. Inventing a second description of the same
//! changes would have meant two encodings to keep in step and two chances to
//! disagree.
//!
//! # Why the envelope exists at all
//!
//! [`RegistryCommand`] wraps a *batch* of ops rather than replicating one at a
//! time, because a single control request can produce several store writes
//! that must land together: a caller that saw `Ok` must not find half of its
//! request replicated. One log entry is one atomic unit here.
//!
//! # Examples
//!
//! ```
//! use astrs_coordinator::ha::RegistryCommand;
//! use astrs_store::record::MutationOp;
//! use astrs_wire::{DataflowId, ParamKey};
//!
//! let command = RegistryCommand::new(vec![MutationOp::ParamDelete {
//!     dataflow: DataflowId::NIL,
//!     key: ParamKey::new("gain")?,
//! }]);
//! assert_eq!(command.len(), 1);
//!
//! let bytes = command.encode()?;
//! assert_eq!(RegistryCommand::decode(&bytes)?, command);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use astrs_store::record::MutationOp;
use astrs_wire::{WireDecode, WireEncode};
use oxicode::{Decode, Encode};

use crate::error::Result;

/// One atomic batch of registry mutations, as replicated through Raft.
///
/// Variant indices are explicit and **append-only**: this is durable state in
/// every replica's Raft log, so it must stay decodable across a coordinator
/// upgrade exactly as a wire enum must (blueprint design principle 4).
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
#[non_exhaustive]
pub enum RegistryCommand {
    /// Apply these operations, in order, as one unit.
    #[oxicode(variant = 0)]
    Mutations(Vec<MutationOp>),
}

impl RegistryCommand {
    /// A command carrying `ops`.
    #[must_use]
    pub const fn new(ops: Vec<MutationOp>) -> Self {
        Self::Mutations(ops)
    }

    /// The operations this command applies.
    #[must_use]
    pub fn ops(&self) -> &[MutationOp] {
        match self {
            Self::Mutations(ops) => ops,
        }
    }

    /// Consumes the command, yielding its operations.
    #[must_use]
    pub fn into_ops(self) -> Vec<MutationOp> {
        match self {
            Self::Mutations(ops) => ops,
        }
    }

    /// How many operations this command carries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ops().len()
    }

    /// Whether this command changes nothing — a batch worth skipping rather
    /// than spending a log entry on.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ops().is_empty()
    }

    /// Encodes this command for the Raft log.
    ///
    /// Uses the same `oxicode` configuration the wire does, so there is
    /// exactly one codec in the process.
    ///
    /// # Errors
    ///
    /// [`crate::CoordinatorError::Wire`] if the batch cannot be encoded.
    pub fn encode(&self) -> Result<Vec<u8>> {
        Ok(self.encode_to_vec()?)
    }

    /// Decodes a command a Raft entry carried.
    ///
    /// # Errors
    ///
    /// [`crate::CoordinatorError::Wire`] if the bytes are not a valid command, or
    /// carry trailing bytes — which would mean a replica in this cluster has
    /// a different idea of the command's shape.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        Ok(Self::decode_exact(bytes)?)
    }
}

impl From<Vec<MutationOp>> for RegistryCommand {
    fn from(ops: Vec<MutationOp>) -> Self {
        Self::new(ops)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use astrs_store::record::ParamRecord;
    use astrs_time::HlcTimestamp;
    use astrs_wire::{DataflowId, ParamKey};

    fn param_put(key: &str, value: &str) -> MutationOp {
        MutationOp::ParamPut {
            dataflow: DataflowId::NIL,
            key: ParamKey::new(key).unwrap(),
            record: ParamRecord {
                value_json: value.to_owned(),
                revision: 1,
                created_at: HlcTimestamp::new(1, 0),
                updated_at: HlcTimestamp::new(1, 0),
            },
        }
    }

    #[test]
    fn a_batch_round_trips_through_the_wire_codec() {
        let command = RegistryCommand::new(vec![
            param_put("a", "1"),
            param_put("b", "2"),
            MutationOp::ParamDelete {
                dataflow: DataflowId::NIL,
                key: ParamKey::new("c").unwrap(),
            },
        ]);
        let bytes = command.encode().unwrap();
        assert_eq!(RegistryCommand::decode(&bytes).unwrap(), command);
        assert_eq!(command.len(), 3);
        assert!(!command.is_empty());
    }

    #[test]
    fn an_empty_batch_is_identifiable_so_it_can_be_skipped() {
        let command = RegistryCommand::new(Vec::new());
        assert!(command.is_empty());
        assert_eq!(command.len(), 0);
        // Still encodable, so a caller that proposes one anyway is not a
        // crash — merely a wasted log entry.
        assert!(command.encode().is_ok());
    }

    #[test]
    fn trailing_bytes_are_refused_rather_than_ignored() {
        // A replica whose idea of the command's shape differs must be found
        // out at the entry that caused it, not somewhere unrelated later.
        let mut bytes = RegistryCommand::new(vec![param_put("a", "1")])
            .encode()
            .unwrap();
        bytes.push(0);
        assert!(RegistryCommand::decode(&bytes).is_err());
    }

    #[test]
    fn a_truncated_command_is_refused() {
        let bytes = RegistryCommand::new(vec![param_put("gain", "1.5")])
            .encode()
            .unwrap();
        assert!(RegistryCommand::decode(&bytes[..bytes.len() - 1]).is_err());
    }

    #[test]
    fn ops_can_be_read_and_taken() {
        let command = RegistryCommand::from(vec![param_put("a", "1")]);
        assert_eq!(command.ops().len(), 1);
        assert_eq!(command.into_ops().len(), 1);
    }

    #[test]
    fn the_encoding_is_deterministic_across_calls() {
        // Every replica must derive byte-identical entries from the same
        // batch, or the log-matching property compares different bytes.
        let command = RegistryCommand::new(vec![param_put("a", "1"), param_put("b", "2")]);
        assert_eq!(command.encode().unwrap(), command.encode().unwrap());
    }
}
