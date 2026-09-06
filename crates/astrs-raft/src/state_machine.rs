//! [`StateMachine`]: what Raft is replicating *to*.
//!
//! Raft itself has no opinion about what a command means. Its contract with
//! the application is exactly three obligations, and they are the three
//! methods of this trait:
//!
//! 1. **Apply committed commands in log order, exactly once.** Every replica
//!    applies the same commands in the same order, so every replica reaches
//!    the same state. [`StateMachine::apply`] is handed the log index too,
//!    because an implementation that persists alongside the log needs to
//!    record how far it got.
//! 2. **Serialize yourself on demand** ([`StateMachine::snapshot`]), so the
//!    log prefix folded into that snapshot can be discarded.
//! 3. **Reconstruct yourself from one** ([`StateMachine::restore`]), so a
//!    replica that fell too far behind can be caught up in one shot instead
//!    of by replaying history it no longer has.
//!
//! # Determinism is the caller's obligation
//!
//! Raft guarantees every replica sees the same command bytes in the same
//! order. It cannot guarantee that applying them produces the same state:
//! that is broken by reading the local clock, a random number, a filesystem
//! or a network. **A command must carry every non-deterministic input it
//! needs, decided once by the leader before it proposed.** The coordinator's
//! own state machine follows exactly this rule — its commands carry complete,
//! already-timestamped records rather than "increment this by one".
//!
//! # Errors are `String`, deliberately
//!
//! An implementor should not have to depend on this crate's error enum to
//! implement one trait; [`crate::RaftError::Apply`] wraps whatever it
//! reports. There is no way for an implementation to say "retry later",
//! because there is none: the entry is already committed and every other
//! replica will apply it.
//!
//! # Examples
//!
//! ```
//! use astrs_raft::{LogIndex, MemoryStateMachine, StateMachine};
//!
//! let mut machine = MemoryStateMachine::new();
//! machine.apply(LogIndex::new(1), b"first")?;
//! machine.apply(LogIndex::new(2), b"second")?;
//!
//! let snapshot = machine.snapshot()?;
//! let mut restored = MemoryStateMachine::new();
//! restored.restore(&snapshot)?;
//! assert_eq!(restored.commands(), machine.commands());
//! # Ok::<(), String>(())
//! ```

use crate::types::LogIndex;

/// The application Raft replicates.
pub trait StateMachine {
    /// Applies one committed command and returns whatever the caller that
    /// proposed it should receive back.
    ///
    /// Called exactly once per committed command, in strictly increasing
    /// index order, on every replica.
    ///
    /// # Errors
    ///
    /// A human-readable description of why the command could not be applied.
    /// The entry is already committed, so this is reported to the caller as
    /// [`crate::RaftError::Apply`] rather than retried.
    fn apply(&mut self, index: LogIndex, command: &[u8]) -> Result<Vec<u8>, String>;

    /// Serializes the machine's entire state.
    ///
    /// # Errors
    ///
    /// A human-readable description of why the state could not be captured.
    fn snapshot(&self) -> Result<Vec<u8>, String>;

    /// Replaces the machine's entire state with a snapshot's.
    ///
    /// # Errors
    ///
    /// A human-readable description of why the snapshot could not be
    /// restored.
    fn restore(&mut self, snapshot: &[u8]) -> Result<(), String>;

    /// The highest index this machine has **durably** applied.
    ///
    /// Raft does not persist `commitIndex`: a restarted replica relearns it
    /// from the next leader and re-applies everything after its snapshot. A
    /// state machine whose own state outlived the process would therefore see
    /// those commands **twice** — and "apply exactly once" is the first thing
    /// this trait promises.
    ///
    /// Two ways to keep that promise, and both are legitimate:
    ///
    /// - **Report a real index.** A machine that persists its state persists
    ///   this alongside it, and [`crate::RaftNode`] resumes from there.
    /// - **Be idempotent and return the default.** A machine whose commands
    ///   are whole-value overwrites (the AstRS coordinator's registry is
    ///   exactly this) can be replayed safely, and the default
    ///   [`crate::LogIndex::ZERO`] says so.
    ///
    /// What is *not* legitimate is a machine that is both durable and
    /// order-dependent returning the default.
    fn applied_index(&self) -> LogIndex {
        LogIndex::ZERO
    }
}

/// A [`StateMachine`] that remembers every command it was given.
///
/// Used by this crate's own simulator to check the *state machine safety*
/// property directly: if two replicas applied different command sequences,
/// their [`MemoryStateMachine::commands`] differ, and the invariant checker
/// says so by name rather than by a downstream symptom.
///
/// # Examples
///
/// ```
/// use astrs_raft::{LogIndex, MemoryStateMachine, StateMachine};
///
/// let mut machine = MemoryStateMachine::new();
/// assert_eq!(machine.apply(LogIndex::new(1), b"hello")?, b"hello".to_vec());
/// assert_eq!(machine.commands().len(), 1);
/// assert_eq!(machine.last_applied(), LogIndex::new(1));
/// # Ok::<(), String>(())
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryStateMachine {
    /// Every command applied, in order.
    commands: Vec<Vec<u8>>,
    /// The index of the last command applied.
    last_applied: LogIndex,
}

impl MemoryStateMachine {
    /// An empty machine.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every command applied so far, in order.
    #[must_use]
    pub fn commands(&self) -> &[Vec<u8>] {
        &self.commands
    }

    /// The index of the last command applied.
    #[must_use]
    pub const fn last_applied(&self) -> LogIndex {
        self.last_applied
    }
}

impl StateMachine for MemoryStateMachine {
    fn apply(&mut self, index: LogIndex, command: &[u8]) -> Result<Vec<u8>, String> {
        self.commands.push(command.to_vec());
        self.last_applied = index;
        Ok(command.to_vec())
    }

    fn snapshot(&self) -> Result<Vec<u8>, String> {
        use astrs_wire::WireEncode;
        (self.last_applied.get(), self.commands.clone())
            .encode_to_vec()
            .map_err(|error| error.to_string())
    }

    fn applied_index(&self) -> LogIndex {
        self.last_applied
    }

    fn restore(&mut self, snapshot: &[u8]) -> Result<(), String> {
        use astrs_wire::WireDecode;
        let (last_applied, commands) =
            <(u64, Vec<Vec<u8>>)>::decode_exact(snapshot).map_err(|error| error.to_string())?;
        self.last_applied = LogIndex::new(last_applied);
        self.commands = commands;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn commands_are_remembered_in_order() {
        let mut machine = MemoryStateMachine::new();
        for (position, command) in [b"a".as_slice(), b"b", b"c"].into_iter().enumerate() {
            machine
                .apply(LogIndex::new(position as u64 + 1), command)
                .unwrap();
        }
        assert_eq!(
            machine.commands(),
            &[b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
        );
        assert_eq!(machine.last_applied(), LogIndex::new(3));
    }

    #[test]
    fn apply_returns_the_result_the_proposer_receives() {
        let mut machine = MemoryStateMachine::new();
        assert_eq!(
            machine.apply(LogIndex::new(1), b"echo").unwrap(),
            b"echo".to_vec()
        );
    }

    #[test]
    fn a_snapshot_round_trips_the_entire_state() {
        let mut source = MemoryStateMachine::new();
        for index in 1..=32u64 {
            source
                .apply(LogIndex::new(index), &index.to_le_bytes())
                .unwrap();
        }
        let bytes = source.snapshot().unwrap();

        let mut restored = MemoryStateMachine::new();
        restored.restore(&bytes).unwrap();
        assert_eq!(restored, source);
        assert_eq!(restored.last_applied(), LogIndex::new(32));
    }

    #[test]
    fn restoring_replaces_rather_than_merges() {
        // A replica catching up from a snapshot must end at the snapshot's
        // state exactly, not at the snapshot plus whatever it had before.
        let mut source = MemoryStateMachine::new();
        source.apply(LogIndex::new(1), b"kept").unwrap();
        let bytes = source.snapshot().unwrap();

        let mut target = MemoryStateMachine::new();
        target.apply(LogIndex::new(1), b"stale").unwrap();
        target.apply(LogIndex::new(2), b"also stale").unwrap();
        target.restore(&bytes).unwrap();

        assert_eq!(target.commands(), &[b"kept".to_vec()]);
        assert_eq!(target.last_applied(), LogIndex::new(1));
    }

    #[test]
    fn an_empty_machine_snapshots_and_restores() {
        let empty = MemoryStateMachine::new();
        let bytes = empty.snapshot().unwrap();
        let mut restored = MemoryStateMachine::new();
        restored.restore(&bytes).unwrap();
        assert_eq!(restored, empty);
    }

    #[test]
    fn a_malformed_snapshot_is_reported_not_ignored() {
        let mut machine = MemoryStateMachine::new();
        let error = machine.restore(b"not a snapshot").unwrap_err();
        assert!(!error.is_empty());
        // Failure leaves the machine untouched rather than half-restored.
        assert!(machine.commands().is_empty());
    }
}
