//! `cargo xtask snapshot-protocol`: verify the wire-protocol freeze
//! (blueprint §7.2, §24.1) without always paying for a full compile.
//!
//! `crates/astrs-wire/tests/protocol_snapshot.rs` is the actual freeze: it
//! renders one deterministic sample per protocol variant from the
//! *compiled, current* `astrs-wire` crate and compares the result against
//! two committed files, answering two different questions (see that file's
//! module docs for the full rationale):
//!
//! | File | Question | Needs the crate compiled? |
//! |---|---|---|
//! | `protocol.snap` | did *anything* change since it was last regenerated? | yes |
//! | `protocol.frozen.snap` | did anything *already frozen* move? | yes, today |
//!
//! This module gives that second question a path that does **not** need
//! `astrs-wire` compiled or run: [`run_frozen_only`] re-implements the same
//! section/prefix comparison the Rust test uses, but applies it to the two
//! **committed files directly** rather than to a freshly rendered
//! snapshot. That is a strictly weaker check -- it can catch someone
//! hand-editing `protocol.snap` in a way that breaks the frozen prefix, but
//! it cannot catch the source code itself drifting from `protocol.snap`,
//! because it never builds or runs the source. [`run_full`] is the
//! authoritative check ([`crate::preflight`] always uses it); this fast
//! path exists for a developer who wants a sub-second sanity check between
//! full runs. [`Outcome::detail`] always says in plain words which of the
//! two just ran -- "keep it honest" is the whole point of having both.

use std::path::Path;
use std::process::Command;

use crate::error::XtaskError;

/// The committed live golden, relative to the workspace root.
pub const PROTOCOL_SNAP: &str = "crates/astrs-wire/tests/golden/protocol.snap";
/// The committed, never-regenerated golden, relative to the workspace
/// root.
pub const PROTOCOL_FROZEN_SNAP: &str = "crates/astrs-wire/tests/golden/protocol.frozen.snap";

/// Which of the two mechanisms produced an [`Outcome`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mechanism {
    /// Compiled and ran `cargo test -p astrs-wire --test protocol_snapshot`
    /// -- the authoritative check.
    FullTest,
    /// Compared the two committed golden files against each other,
    /// on-disk text only. No compilation, no execution of `astrs-wire`.
    FrozenOnly,
}

impl std::fmt::Display for Mechanism {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FullTest => write!(
                f,
                "compiled and ran `cargo test -p astrs-wire --test protocol_snapshot` \
                 (authoritative: verifies the compiled crate against both goldens)"
            ),
            Self::FrozenOnly => write!(
                f,
                "static comparison of the two committed golden files only -- no compile, \
                 no execution of astrs-wire, so drift between the current source and \
                 protocol.snap is NOT detected by this fast path"
            ),
        }
    }
}

/// The result of one check, honest about which [`Mechanism`] produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Which mechanism ran.
    pub mechanism: Mechanism,
    /// Whether it passed.
    pub passed: bool,
    /// A human-readable detail line: what ran (or what broke).
    pub detail: String,
}

/// Run the authoritative check: compile and run `astrs-wire`'s own
/// `protocol_snapshot` test binary as a subprocess, forwarding its exit
/// status. Cargo's own test output streams straight through (this
/// function inherits the child's stdio), so a failure's full diagnostic --
/// the exact line that moved -- reaches whoever ran `cargo xtask
/// snapshot-protocol` without this module having to re-render it.
///
/// # Errors
///
/// [`XtaskError::Spawn`] if `cargo` itself could not be started (not
/// found, OS refused). A `cargo test` that runs and fails is not an error:
/// it is `Ok(Outcome { passed: false, .. })`.
pub fn run_full(root: &Path) -> Result<Outcome, XtaskError> {
    const ARGS: [&str; 5] = ["test", "-p", "astrs-wire", "--test", "protocol_snapshot"];
    let status = Command::new("cargo")
        .args(ARGS)
        .current_dir(root)
        .status()
        .map_err(|source| XtaskError::spawn(format!("cargo {}", ARGS.join(" ")), source))?;
    Ok(Outcome {
        mechanism: Mechanism::FullTest,
        passed: status.success(),
        detail: format!("cargo {} exited {status}", ARGS.join(" ")),
    })
}

/// Run the fast path: compare the two committed golden files against each
/// other, on disk, without building or running anything. See the module
/// docs for exactly what this does and does not prove.
///
/// # Errors
///
/// [`XtaskError::Io`] if either golden file cannot be read.
pub fn run_frozen_only(root: &Path) -> Result<Outcome, XtaskError> {
    let snap_path = root.join(PROTOCOL_SNAP);
    let frozen_path = root.join(PROTOCOL_FROZEN_SNAP);
    let snap_text =
        std::fs::read_to_string(&snap_path).map_err(|source| XtaskError::io(&snap_path, source))?;
    let frozen_text = std::fs::read_to_string(&frozen_path)
        .map_err(|source| XtaskError::io(&frozen_path, source))?;
    Ok(compare_committed(&frozen_text, &snap_text))
}

/// The comparison behind [`run_frozen_only`], as a pure function over text
/// -- so it is testable with small synthetic snapshots rather than only
/// against whatever the committed files happen to say today.
fn compare_committed(frozen_text: &str, live_text: &str) -> Outcome {
    let frozen = sections(frozen_text);
    let live = sections(live_text);
    match first_prefix_break(&frozen, &live) {
        None => Outcome {
            mechanism: Mechanism::FrozenOnly,
            passed: true,
            detail: format!(
                "every frozen section is a prefix of the committed protocol.snap. {}",
                Mechanism::FrozenOnly
            ),
        },
        Some(break_) => Outcome {
            mechanism: Mechanism::FrozenOnly,
            passed: false,
            detail: format!("the frozen prefix moved: {break_}"),
        },
    }
}

/// Why a live snapshot is not an append-only successor of the frozen one.
/// Ported from `crates/astrs-wire/tests/protocol_snapshot.rs`'s identical
/// type -- see that file's module docs for the full rationale; this is the
/// same shape of break, just detected between two committed files instead
/// of between the frozen file and a freshly rendered one.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PrefixBreak {
    /// A whole family disappeared.
    SectionMissing(String),
    /// A family lost lines: a frozen variant was removed.
    SectionShrank(String),
    /// A frozen line moved: a renumber, a rename, a reordered or widened
    /// field.
    LineMoved {
        section: String,
        index: usize,
        frozen: String,
        live: String,
    },
    /// A family the frozen copy has never seen.
    SectionNew(String),
}

impl std::fmt::Display for PrefixBreak {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SectionMissing(name) => {
                write!(f, "section [{name}] is missing from protocol.snap")
            }
            Self::SectionShrank(name) => {
                write!(f, "section [{name}] has fewer lines than the frozen copy")
            }
            Self::LineMoved {
                section,
                index,
                frozen,
                live,
            } => write!(
                f,
                "section [{section}] line {index}: frozen has `{frozen}`, protocol.snap has \
                 `{live}`"
            ),
            Self::SectionNew(name) => write!(f, "section [{name}] is new (fine if intentional)"),
        }
    }
}

/// Splits a rendered snapshot into `section name -> its lines`, in file
/// order. Comment and blank lines are dropped. Identical logic to the
/// astrs-wire test's own `sections` helper.
fn sections(snapshot: &str) -> Vec<(String, Vec<&str>)> {
    let mut sections: Vec<(String, Vec<&str>)> = Vec::new();
    for line in snapshot.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            let name = line
                .split(']')
                .next()
                .unwrap_or(line)
                .trim_start_matches('[')
                .to_owned();
            sections.push((name, vec![line]));
        } else if let Some((_, lines)) = sections.last_mut() {
            lines.push(line);
        }
    }
    sections
}

/// The first way `live` fails to be an append-only successor of `frozen`.
/// Identical logic to the astrs-wire test's own `first_prefix_break`.
fn first_prefix_break(
    frozen: &[(String, Vec<&str>)],
    live: &[(String, Vec<&str>)],
) -> Option<PrefixBreak> {
    for (name, frozen_lines) in frozen {
        let Some((_, live_lines)) = live.iter().find(|(live_name, _)| live_name == name) else {
            return Some(PrefixBreak::SectionMissing(name.clone()));
        };
        if live_lines.len() < frozen_lines.len() {
            return Some(PrefixBreak::SectionShrank(name.clone()));
        }
        for (index, frozen_line) in frozen_lines.iter().enumerate() {
            if live_lines[index] != *frozen_line {
                return Some(PrefixBreak::LineMoved {
                    section: name.clone(),
                    index,
                    frozen: (*frozen_line).to_owned(),
                    live: live_lines[index].to_owned(),
                });
            }
        }
    }
    for (name, _) in live {
        if !frozen.iter().any(|(frozen_name, _)| frozen_name == name) {
            return Some(PrefixBreak::SectionNew(name.clone()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    const BASE: &str = "\
# a comment, ignored\n\
\n\
[Alpha] kind=1\n\
0 First aa\n\
1 Second bb\n\
\n\
[Beta] kind=2\n\
0 Only cc\n";

    #[test]
    fn identical_text_has_no_break() {
        let outcome = compare_committed(BASE, BASE);
        assert!(outcome.passed);
        assert_eq!(outcome.mechanism, Mechanism::FrozenOnly);
    }

    #[test]
    fn a_tail_append_within_an_existing_section_has_no_break() {
        // Adding a new variant at the end of an *existing* section is
        // exactly what an intended, wire-compatible change looks like.
        let appended = format!("{BASE}1 Second-of-Beta dd\n");
        let outcome = compare_committed(BASE, &appended);
        assert!(outcome.passed, "{outcome:?}");
    }

    #[test]
    fn a_brand_new_section_is_flagged_until_frozen_records_it_too() {
        // A whole new family is `PrefixBreak::SectionNew` -- and, exactly
        // like the real `the_frozen_prefix_of_every_family_is_unchanged`
        // test in `protocol_snapshot.rs`, that still fails this check.
        // Its own doc comment says why: "fine to add, but record it in
        // protocol.frozen.snap in the same commit as protocol.snap" --
        // "fine" describes the *change*, not this comparison's verdict on
        // a frozen copy that has not been updated yet. Passing here would
        // make `--frozen-only` disagree with the authoritative
        // `cargo test -p astrs-wire --test protocol_snapshot`, which does
        // fail in this situation.
        let with_new_section = format!("{BASE}\n[Gamma] kind=3\n0 New ee\n");
        let outcome = compare_committed(BASE, &with_new_section);
        assert!(!outcome.passed);
        assert!(outcome.detail.contains("Gamma"));
    }

    #[test]
    fn a_moved_line_is_a_break() {
        // Same shape as `the_frozen_prefix_guard_notices_a_renumber` in
        // the real astrs-wire test: swap two frozen lines.
        let reordered = "\
[Alpha] kind=1\n\
0 Second bb\n\
1 First aa\n\
\n\
[Beta] kind=2\n\
0 Only cc\n";
        let outcome = compare_committed(BASE, reordered);
        assert!(!outcome.passed);
        assert!(outcome.detail.contains("Alpha"));
    }

    #[test]
    fn a_shrunk_section_is_a_break() {
        let shrunk = "[Alpha] kind=1\n0 First aa\n\n[Beta] kind=2\n0 Only cc\n";
        let outcome = compare_committed(BASE, shrunk);
        assert!(!outcome.passed);
        assert!(outcome.detail.contains("Alpha"));
    }

    #[test]
    fn a_missing_section_is_a_break() {
        let without_beta = "[Alpha] kind=1\n0 First aa\n1 Second bb\n";
        let outcome = compare_committed(BASE, without_beta);
        assert!(!outcome.passed);
        assert!(outcome.detail.contains("Beta"));
    }

    #[test]
    fn passing_detail_still_names_the_mechanism_honestly() {
        let outcome = compare_committed(BASE, BASE);
        assert!(outcome.detail.contains("no compile"));
    }

    #[test]
    fn full_test_mechanism_message_says_what_it_verifies() {
        assert!(Mechanism::FullTest.to_string().contains("authoritative"));
        assert!(Mechanism::FrozenOnly.to_string().contains("NOT detected"));
    }

    #[test]
    fn run_frozen_only_reads_the_real_committed_goldens_and_they_agree() {
        // The two real files are, today, exactly what protocol_snapshot.rs
        // asserts they are: the frozen file's every section is a prefix of
        // the live one's. This exercises the real file-reading path, on
        // top of the synthetic-text tests above that exercise the compare
        // logic's edge cases (see the module docs on why this fast path
        // does not, by itself, prove the crate still compiles to match).
        let root = crate::workspace::workspace_root();
        let outcome = run_frozen_only(&root).unwrap();
        assert!(outcome.passed, "{outcome:?}");
        assert_eq!(outcome.mechanism, Mechanism::FrozenOnly);
    }

    #[test]
    fn run_frozen_only_reports_a_missing_golden_as_an_io_error() {
        let root = std::env::temp_dir().join(format!(
            "astrs-xtask-snapshot-protocol-test-missing-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let err = run_frozen_only(&root).unwrap_err();
        assert!(matches!(err, XtaskError::Io { .. }));
    }
}
