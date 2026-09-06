//! The verbs blueprint §17 lists that this build does not implement yet,
//! and the one place that says which wave each of them belongs to.
//!
//! Every verb in the §17 table parses with its real argument schema — so
//! `--help`, shell completion and argument validation describe the whole
//! surface — and all but these reach a real implementation. What is left
//! here is nothing at all: **every** §17 verb now reaches a real
//! implementation in `crate::dispatch`, and [`STUBBED_VERBS`] is empty.
//!
//! The module is kept rather than deleted because the *mechanism* — a verb
//! that parses with its real argument schema, reports
//! [`crate::error::CliError::NotImplementedYet`] naming the wave that owns
//! it, and exits [`crate::error::EXIT_UNAVAILABLE`] so a script can tell it
//! apart from a failure — is the right shape for the next crate this build
//! predates, and re-deriving it later would be worse than keeping four
//! constants and a lookup.
//!
//! # What left this table, wave by wave
//!
//! `record start`, `record stop`, `replay` and `bag info` used to be listed
//! here. The recorder (`astrs-recording`, with `.arec` reader, writer and
//! seekable index) landed in W4, so all four now dispatch to
//! [`crate::command::record`], [`crate::command::replay`] and
//! [`crate::command::bag`].
//!
//! `bag convert` stayed behind through W4 because converting *into*
//! rosbag2 needs a writer, not just the reader `bag info` used at the
//! time. The `astrs-rosbag` wave gave it both a `.db3` writer and
//! `.arec ⇄ bag` conversion, so `bag convert` now dispatches to
//! [`crate::command::bag::convert`] as well, and `bag info` was extended
//! to read `.db3`/`.mcap` too, not just `.arec`.
//!
//! `ros2 doctor` and `ros2 topics` were the last entries. The ROS 2 interop
//! wave (`astrs-rtps`/`astrs-ros2`) landed, and both now spin a real probe
//! participant through [`crate::command::ros2`] — so the table emptied in
//! the same change that wired them up, which is the rule this module's own
//! docs state.
//!
//! Two limits inside those implementations are reported as ordinary
//! argument errors rather than from here, because the verb does run:
//! `astrs replay` requires `--into <manifest.yml>` (replaying straight into
//! a *running* dataflow is not implemented), and `astrs run --deterministic`
//! is refused before anything starts rather than producing an ordinary run
//! wearing a deterministic label.
//!
//! Naming the wave in the error is the point: a user who types a verb the
//! `--help` output advertises deserves to know *when* it arrives, not just
//! that it is missing, and a script can tell this apart from an ordinary
//! failure by [`crate::error::EXIT_UNAVAILABLE`].

use std::io::Write;

use crate::error::CliError;

/// The example wave string [`crate::error::CliError::NotImplementedYet`]'s
/// own docs cite. No verb in [`STUBBED_VERBS`] waits on it any more — the
/// `astrs-rosbag` wave gave `bag convert` (the sole verb that used to)
/// its real implementation — but the constant is kept rather than
/// removed, since a future crate this build still predates may need
/// exactly this shape again, and `error.rs`'s doctest-adjacent example
/// keeps citing it as a realistic-looking wave label.
pub const WAVE_BAG: &str = "the `astrs-rosbag` wave";

/// The wave that owned the ROS 2 interop stack.
///
/// Kept for the same reason as [`WAVE_BAG`]: `ros2 doctor`/`ros2 topics`
/// were its last two entries and both now have implementations, but the
/// label is the realistic-looking example this module's docs and tests
/// cite.
pub const WAVE_ROS2: &str = "the ROS 2 interop wave (`astrs-rtps`/`astrs-ros2`)";

/// Every verb `crate::dispatch` still routes here, with the wave that owns it.
///
/// **Empty.** This is the authoritative table: [`wave_for`] reads it, the
/// module documentation mirrors it, and a verb that gains an implementation
/// must leave here in the same change that wires it up. Every §17 verb has
/// one, so nothing is listed.
pub const STUBBED_VERBS: [(&str, &str); 0] = [];

/// The wave a not-yet-implemented verb belongs to.
///
/// An unknown verb name (a caller mistake, since every string passed here
/// is a literal in [`crate::dispatch`]) is reported as belonging to a later
/// wave rather than claiming a specific one — the honest answer when the
/// table has no entry. A verb that *used* to be stubbed and now has an
/// implementation falls into that case too, and never reaches this function.
#[must_use]
pub fn wave_for(verb: &str) -> &'static str {
    let mut index = 0;
    while index < STUBBED_VERBS.len() {
        let (name, wave) = STUBBED_VERBS[index];
        if name == verb {
            return wave;
        }
        index += 1;
    }
    "a later wave"
}

/// Report that `verb` is not implemented in this build, naming the wave it
/// is scheduled for.
///
/// # Errors
///
/// Always returns [`CliError::NotImplementedYet`].
pub fn run(_out: &mut dyn Write, verb: &'static str) -> Result<(), CliError> {
    Err(CliError::NotImplementedYet {
        verb,
        wave: wave_for(verb),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    /// The mechanism still works for a verb a future wave adds — which is
    /// the whole reason this module outlived its table.
    #[test]
    fn stub_names_the_invoked_verb_and_a_wave() {
        let err = run(&mut Vec::new(), "some future verb").unwrap_err();
        match err {
            CliError::NotImplementedYet { verb, wave } => {
                assert_eq!(verb, "some future verb");
                assert_eq!(wave, "a later wave");
            }
            other => panic!("expected NotImplementedYet, got {other:?}"),
        }
    }

    #[test]
    fn stub_uses_the_unavailable_exit_code() {
        let err = run(&mut Vec::new(), "some future verb").unwrap_err();
        assert_eq!(err.exit_code(), crate::error::EXIT_UNAVAILABLE);
    }

    #[test]
    fn different_verbs_produce_distinguishable_errors() {
        let first = run(&mut Vec::new(), "verb one").unwrap_err();
        let second = run(&mut Vec::new(), "verb two").unwrap_err();
        assert_ne!(first.to_string(), second.to_string());
    }

    /// The table is empty, and that is the assertion: a verb added back to
    /// it without an implementation would fail here.
    #[test]
    fn every_section_seventeen_verb_has_an_implementation() {
        assert!(
            STUBBED_VERBS.is_empty(),
            "STUBBED_VERBS still lists {STUBBED_VERBS:?}"
        );
        assert_eq!(
            WAVE_ROS2,
            "the ROS 2 interop wave (`astrs-rtps`/`astrs-ros2`)"
        );
    }

    #[test]
    fn every_still_missing_verb_has_a_wave_of_its_own() {
        for (verb, wave) in STUBBED_VERBS {
            assert_eq!(wave_for(verb), wave, "{verb}");
        }
    }

    /// The drift guard: each wave listed gave these verbs real
    /// implementations, so naming any of them here again would mean the
    /// table outlived the code it describes — which is exactly the bug
    /// this test was written for.
    #[test]
    fn the_verbs_already_implemented_are_no_longer_claimed_as_missing() {
        for implemented in [
            "record start",
            "record stop",
            "replay",
            "bag info",
            "bag convert",
            "ros2 doctor",
            "ros2 topics",
        ] {
            assert!(
                !STUBBED_VERBS.iter().any(|(verb, _)| *verb == implemented),
                "`{implemented}` has an implementation; it must not be in STUBBED_VERBS"
            );
            assert_eq!(wave_for(implemented), "a later wave");
        }
    }

    #[test]
    fn the_table_names_each_verb_once() {
        for (index, (verb, _)) in STUBBED_VERBS.iter().enumerate() {
            assert!(
                !STUBBED_VERBS[..index].iter().any(|(seen, _)| seen == verb),
                "`{verb}` appears twice"
            );
        }
    }

    #[test]
    fn an_unlisted_verb_claims_no_particular_wave() {
        assert_eq!(wave_for("something-else"), "a later wave");
    }
}
