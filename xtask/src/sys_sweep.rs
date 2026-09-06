//! `cargo xtask preflight`'s `*-sys` sweep: catch a raw-FFI-binding crate
//! entering the graph outside the short, hand-verified allowlist (COOLJAPAN
//! policy's "most `-sys` (FFI) crates" line, ~/.claude/CLAUDE.md).
//!
//! # Why this is "in-process" like [`crate::layer_lint`], not just another
//! `cargo` step
//!
//! Every other subprocess-backed preflight step (`cargo fmt --check`,
//! `cargo clippy`, `cargo deny check bans`, ...; see [`crate::preflight`])
//! is a pure pass/fail wrapper: the external tool's own exit status *is*
//! the verdict, and its stdout/stderr stream straight to the terminal
//! uninspected. `cargo tree` has no concept of an allowlist -- it exits `0`
//! for any graph it can resolve, disallowed crates included -- so the
//! actual check here is this module's own Rust code comparing `cargo
//! tree`'s output against [`ALLOWLIST`], the same shape as `layer_lint`
//! applying its own upward/sideways rule over parsed `Cargo.toml` data
//! rather than trusting an external tool's exit code.
//!
//! # `-e normal`: what this sweep does and does not cover
//!
//! `-e normal` (matching the task's own given command) restricts the walk
//! to `[dependencies]` edges -- `[build-dependencies]` and
//! `[dev-dependencies]` are out of scope for this sweep. That is not an
//! oversight: a `-sys` crate reached only through a build script or a test
//! harness never ships inside a released AstRS binary, which is what this
//! sweep exists to keep clean, and `deny.toml`'s `cc` ban entry (with its
//! `wrappers` allowlist) already separately covers the build-time C-compile
//! question `[build-dependencies]` would raise.
//!
//! # Why `--target all`
//!
//! The task's own verified allowlist -- `windows-sys`, `libc`,
//! `linux-raw-sys`, `core-foundation-sys`, `js-sys`, `web-sys` -- names
//! crates that resolve on four different platform families (Windows,
//! any-Unix, Linux specifically, wasm32). `cargo tree` with no `--target`
//! flag resolves only the *host* target it runs on: on the macOS box this
//! was developed on, a plain run sees exactly `core-foundation-sys` (and
//! `libc`, pulled in by ordinary Unix-target crates like `rustix`/`tokio`)
//! -- `windows-sys`/`linux-raw-sys`/`js-sys`/`web-sys` are invisible, not
//! merely absent, because nothing ever resolves a Windows- or wasm-only
//! dependency edge for a macOS target. `--target all` resolves the graph
//! for every platform Cargo.lock carries a target-specific answer for
//! (matching this workspace's own established methodology -- see
//! `deny.toml`'s `cc` entry and the root `Cargo.toml`'s Arrow section, both
//! of which cite `cargo tree ... --target all` for exactly this reason),
//! so the sweep catches a Windows-only or wasm-only `-sys` crate regardless
//! of which platform `cargo xtask preflight` itself happens to run on.
//! Verified empirically: `cargo tree -e normal --all-features --workspace
//! --target all --prefix none`, filtered to `-sys`/`libc` names, produces
//! *exactly* [`ALLOWLIST`]'s six entries against the real workspace and
//! nothing else.
//!
//! # `libc` without a `-sys` suffix
//!
//! [`ALLOWLIST`] is not a pure `*-sys` name filter: `libc` carries the
//! identical raw-FFI-declarations shape (see its own allowlist reason
//! below) without the naming convention's suffix, so [`is_sys_shaped`]
//! matches it explicitly by name alongside the suffix check. Every other
//! crate in the resolved graph -- `-sys`-suffixed or not -- is left alone.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use crate::error::XtaskError;

/// Which optional-dependency edges a `cargo tree` resolution activates.
/// Mirrors [`crate::preflight`]'s default-features/`--all-features` pair
/// for `cargo deny check bans` (see that module's
/// `deny_bans_all_features_invocation` doc comment for the identical
/// reasoning): a `-sys` crate hiding behind a feature only one of the two
/// modes activates -- `astrs-transport`'s `quic` feature is exactly such a
/// gate today -- needs both runs to be caught.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeatureMode {
    /// `--no-default-features`: the leanest graph every member can resolve
    /// to.
    NoDefaultFeatures,
    /// `--all-features`: every optional edge activated at once.
    AllFeatures,
}

impl FeatureMode {
    /// A short, human-readable label for step names and messages.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::NoDefaultFeatures => "no-default-features",
            Self::AllFeatures => "all-features",
        }
    }

    /// The `cargo tree` argv for this mode -- see the module docs for why
    /// each flag (`-e normal`, `--target all`, `--prefix none`) is there.
    fn cargo_tree_args(self) -> Vec<&'static str> {
        let mut args = vec![
            "tree",
            "-e",
            "normal",
            "--workspace",
            "--target",
            "all",
            "--prefix",
            "none",
        ];
        args.push(match self {
            Self::NoDefaultFeatures => "--no-default-features",
            Self::AllFeatures => "--all-features",
        });
        args
    }
}

/// Every raw-FFI-binding crate this sweep has hand-verified as C-free --
/// `use-instead`-style `deny.toml` entries would say "N/A", since none of
/// these has (or should have) a pure-Rust replacement; the reason each is
/// safe is instead *why* it never compiles or links C, which the paired
/// comment states.
const ALLOWLIST: &[(&str, &str)] = &[
    (
        "windows-sys",
        "Bindgen'd Windows syscall/ABI declarations only (Microsoft's windows-rs project) -- \
         no C compiled.",
    ),
    (
        "libc",
        "Raw libc FFI declarations only -- no C compiled; the crate every Rust binary on a \
         Unix-like or Windows target already links the platform C library through.",
    ),
    (
        "linux-raw-sys",
        "Bindgen'd Linux syscall ABI declarations, generated straight from the kernel's own \
         headers -- no C compiled; rustix's Linux backend.",
    ),
    (
        "core-foundation-sys",
        "Framework-link only -- macOS/iOS CoreFoundation.framework bindings, no C compiled.",
    ),
    (
        "js-sys",
        "JS bindings for the wasm32 target -- no C compiled, nothing to link but the host \
         JS engine.",
    ),
    (
        "web-sys",
        "Web API bindings for the wasm32 target -- no C compiled, nothing to link but the \
         host JS engine.",
    ),
];

/// Whether `name` is shaped like a raw-FFI-binding crate this sweep cares
/// about -- the `*-sys` naming convention, plus `libc` itself (see the
/// module docs' "`libc` without a `-sys` suffix" section).
fn is_sys_shaped(name: &str) -> bool {
    name.ends_with("-sys") || name == "libc"
}

/// Whether `name` is one of [`ALLOWLIST`]'s hand-verified entries.
fn is_allowlisted(name: &str) -> bool {
    ALLOWLIST.iter().any(|(allowed, _reason)| *allowed == name)
}

/// Every FFI/binding-shaped crate name [`is_sys_shaped`] matches in one
/// `cargo tree --prefix none` run's stdout, deduplicated.
///
/// Pure text processing, no knowledge of `cargo`'s invocation -- the piece
/// of this sweep [`crate::sys_sweep`]'s own tests exercise directly, the
/// same split [`crate::layer_lint::lint`] keeps from [`crate::layer_lint::run`].
/// `--prefix none` output is one `<name> v<version>[ (*)][ (path)]` line
/// per graph edge (root-package lines and duplicate-subtree markers
/// included), separated by blank lines between each workspace member's own
/// tree section -- taking each line's first whitespace-delimited token is
/// enough to recover the crate name regardless of which of those shapes a
/// given line is.
fn sys_shaped_crate_names(tree_stdout: &str) -> BTreeSet<String> {
    tree_stdout
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| is_sys_shaped(name))
        .map(str::to_owned)
        .collect()
}

/// Every FFI/binding-shaped crate name in `tree_stdout` that is not in
/// [`ALLOWLIST`], sorted (filtering a [`BTreeSet`]'s iterator, as
/// [`sys_shaped_crate_names`] returns, preserves its ascending order, so no
/// separate sort is needed here). The pure decision layer behind [`run`] --
/// split out expressly so this module's tests can exercise "given this
/// resolved graph, which crates would be flagged" without spawning a real
/// `cargo tree` for every case, the same split [`sys_shaped_crate_names`]
/// itself already is.
fn disallowed_crate_names(tree_stdout: &str) -> Vec<String> {
    sys_shaped_crate_names(tree_stdout)
        .into_iter()
        .filter(|name| !is_allowlisted(name))
        .collect()
}

/// Spawn `cargo <args>` in `root`, capturing stdout as UTF-8 (lossily --
/// crate names are always ASCII in practice, and a lossy replacement
/// character would itself just fail the allowlist match rather than panic
/// or silently misparse). Distinct from [`crate::preflight`]'s
/// `spawn_and_capture`: that one only reports pass/fail and lets a step's
/// own stdout/stderr stream straight to the terminal (the right shape for
/// `clippy`/`nextest`, whose *output itself* is the diagnostic); this one
/// has to read `cargo tree`'s stdout back into the process to search it,
/// so it captures rather than inherits.
///
/// # Errors
///
/// [`XtaskError::Spawn`] if `cargo` itself could not be started.
/// [`XtaskError::CommandFailed`] if it started but exited non-zero -- see
/// that variant's doc comment for why that is xtask's-own-machinery-failed
/// rather than a sweep finding.
fn cargo_tree_stdout(root: &Path, args: &[&str]) -> Result<String, XtaskError> {
    let command_label = format!("cargo {}", args.join(" "));
    let output = Command::new("cargo")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|source| XtaskError::spawn(command_label.clone(), source))?;
    if !output.status.success() {
        return Err(XtaskError::command_failed(
            command_label,
            &String::from_utf8_lossy(&output.stderr),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The result of one [`run`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepReport {
    /// Which dependency-resolution mode this report covers.
    pub mode: FeatureMode,
    /// Every `-sys`/`libc`-shaped crate name `cargo tree` resolved that is
    /// not in [`ALLOWLIST`], sorted. Empty means clean.
    pub disallowed: Vec<String>,
}

impl SweepReport {
    /// Whether this run found nothing to report.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.disallowed.is_empty()
    }
}

/// Resolve the workspace's dependency graph under `mode` via `cargo tree`
/// and check every raw-FFI-binding crate name it contains against
/// [`ALLOWLIST`]. See the module docs for the full rationale behind every
/// flag this passes to `cargo tree`.
///
/// # Errors
///
/// [`XtaskError::Spawn`] or [`XtaskError::CommandFailed`] -- see
/// [`cargo_tree_stdout`]. Never for a disallowed crate being found; that is
/// `Ok(SweepReport { disallowed: [..], .. })`, matching how every other
/// preflight check in this crate treats "ran and found a violation" as a
/// successful, reportable outcome rather than a failure of xtask itself.
pub fn run(root: &Path, mode: FeatureMode) -> Result<SweepReport, XtaskError> {
    let stdout = cargo_tree_stdout(root, &mode.cargo_tree_args())?;
    Ok(SweepReport {
        mode,
        disallowed: disallowed_crate_names(&stdout),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    // -- Allowlist shape: every entry is genuinely `-sys`-shaped or `libc`,
    // -- so a typo in the table itself (e.g. an entry that could never be
    // -- matched by `is_sys_shaped`) is caught here rather than silently
    // -- making that entry dead weight.

    #[test]
    fn every_allowlist_entry_is_sys_shaped() {
        for (name, _reason) in ALLOWLIST {
            assert!(is_sys_shaped(name), "{name} does not look -sys-shaped");
        }
    }

    #[test]
    fn every_allowlist_entry_has_a_non_empty_reason() {
        for (name, reason) in ALLOWLIST {
            assert!(!reason.is_empty(), "{name} has an empty allowlist reason");
        }
    }

    // -- `is_sys_shaped` / `is_allowlisted`: the actual predicate.

    #[test]
    fn sys_suffixed_names_are_sys_shaped() {
        assert!(is_sys_shaped("openssl-sys"));
        assert!(is_sys_shaped("zstd-sys"));
        assert!(is_sys_shaped("windows-sys"));
    }

    #[test]
    fn libc_is_sys_shaped_without_the_suffix() {
        assert!(is_sys_shaped("libc"));
    }

    #[test]
    fn an_ordinary_crate_is_not_sys_shaped() {
        assert!(!is_sys_shaped("serde"));
        assert!(!is_sys_shaped("tokio"));
        // Must not false-positive on mere substring containment.
        assert!(!is_sys_shaped("system-config")); // contains "sys" but not "-sys"-suffixed
        assert!(!is_sys_shaped("libcurl")); // starts like "libc" but is not it
    }

    #[test]
    fn allowlist_membership_is_exact_and_case_sensitive() {
        assert!(is_allowlisted("libc"));
        assert!(is_allowlisted("core-foundation-sys"));
        assert!(!is_allowlisted("openssl-sys"));
        assert!(!is_allowlisted("Libc"));
    }

    // -- `sys_shaped_crate_names`: parsing `--prefix none` output, entirely
    // -- offline (canned text, never a real `cargo tree` invocation -- see
    // -- `preflight`'s own module docs for why this crate's tests avoid
    // -- spawning heavy real cargo commands; `run`'s real-subprocess path
    // -- is covered by `the_real_workspace_sys_sweep_is_clean` below, and
    // -- `cargo tree` is cheap -- graph resolution only, no compilation --
    // -- unlike the workspace-wide builds that module's docs are about).

    #[test]
    fn parses_plain_prefix_none_lines() {
        let stdout = "astrs-cli v0.1.0 (/repo/bins/astrs-cli)\n\
                       ├── tokio v1.53.1\n\
                       serde v1.0.229\n\
                       libc v0.2.189\n\
                       core-foundation-sys v0.8.7\n";
        // This fixture keeps one raw tree-drawing-prefixed line
        // (`├── tokio ...`) specifically to prove the parser only ever
        // looks at `--prefix none` output shaped lines -- it is not
        // expected to strip box-drawing characters itself, so that line's
        // first whitespace token is the prefix glyph, not a crate name,
        // and must not appear in the result.
        let names = sys_shaped_crate_names(stdout);
        assert_eq!(
            names,
            BTreeSet::from(["libc".to_owned(), "core-foundation-sys".to_owned()])
        );
    }

    #[test]
    fn deduplicates_repeated_names_and_ignores_the_repeat_marker() {
        let stdout = "libc v0.2.189\n\
                       libc v0.2.189 (*)\n\
                       libc v0.2.189 (*)\n";
        assert_eq!(
            sys_shaped_crate_names(stdout),
            BTreeSet::from(["libc".to_owned()])
        );
    }

    #[test]
    fn blank_separator_lines_between_workspace_members_are_ignored() {
        // `cargo tree --workspace` prints one tree section per member,
        // blank-line separated -- must not panic or fabricate a name.
        let stdout = "astrs-cli v0.1.0 (/repo/bins/astrs-cli)\n\
                       libc v0.2.189\n\
                       \n\
                       astrs-tui v0.1.0 (/repo/crates/astrs-tui)\n\
                       core-foundation-sys v0.8.7\n";
        assert_eq!(
            sys_shaped_crate_names(stdout),
            BTreeSet::from(["libc".to_owned(), "core-foundation-sys".to_owned()])
        );
    }

    #[test]
    fn ordinary_dependencies_never_match() {
        let stdout = "astrs-cli v0.1.0\n\
                       serde v1.0.229\n\
                       tokio v1.53.1\n";
        assert!(sys_shaped_crate_names(stdout).is_empty());
    }

    // -- `disallowed_crate_names`: the actual flagging decision, given a
    // -- resolved graph -- the one piece of `run`'s own logic that neither
    // -- `sys_shaped_crate_names`'s parsing tests nor `is_allowlisted`'s
    // -- membership tests exercise end to end (an inverted `!` here, for
    // -- example, would flag nothing while every other test above still
    // -- passes; the real-workspace tests further below cannot catch it
    // -- either, since the real graph is clean by construction).

    #[test]
    fn disallowed_crate_names_flags_only_the_non_allowlisted_ones() {
        let stdout = "astrs-cli v0.1.0 (/repo/bins/astrs-cli)\n\
                       libc v0.2.189\n\
                       core-foundation-sys v0.8.7\n\
                       openssl-sys v0.9.109\n\
                       zstd-sys v2.0.16\n";
        assert_eq!(
            disallowed_crate_names(stdout),
            vec!["openssl-sys".to_owned(), "zstd-sys".to_owned()]
        );
    }

    #[test]
    fn disallowed_crate_names_is_empty_when_everything_is_allowlisted() {
        let stdout = "libc v0.2.189\nwindows-sys v0.61.2\n";
        assert!(disallowed_crate_names(stdout).is_empty());
    }

    #[test]
    fn disallowed_crate_names_sorts_its_output() {
        let stdout = "zstd-sys v2.0.16\nopenssl-sys v0.9.109\nbz2-sys v0.1.0\n";
        assert_eq!(
            disallowed_crate_names(stdout),
            vec![
                "bz2-sys".to_owned(),
                "openssl-sys".to_owned(),
                "zstd-sys".to_owned(),
            ]
        );
    }

    // -- `run`'s pure decision layer, via a hand-built report rather than
    // -- `run` itself (which needs a real `cargo tree`; see above).

    #[test]
    fn is_clean_reflects_an_empty_disallowed_list() {
        let clean = SweepReport {
            mode: FeatureMode::AllFeatures,
            disallowed: Vec::new(),
        };
        assert!(clean.is_clean());

        let dirty = SweepReport {
            mode: FeatureMode::AllFeatures,
            disallowed: vec!["openssl-sys".to_owned()],
        };
        assert!(!dirty.is_clean());
    }

    #[test]
    fn cargo_tree_args_cover_both_modes() {
        let no_default = FeatureMode::NoDefaultFeatures.cargo_tree_args();
        assert_eq!(
            no_default,
            vec![
                "tree",
                "-e",
                "normal",
                "--workspace",
                "--target",
                "all",
                "--prefix",
                "none",
                "--no-default-features"
            ]
        );
        let all_features = FeatureMode::AllFeatures.cargo_tree_args();
        assert_eq!(
            all_features,
            vec![
                "tree",
                "-e",
                "normal",
                "--workspace",
                "--target",
                "all",
                "--prefix",
                "none",
                "--all-features"
            ]
        );
    }

    #[test]
    fn cargo_tree_stdout_reports_command_failed_when_cargo_exits_non_zero() {
        // A directory with no `Cargo.toml` in it or any parent makes a
        // real `cargo` spawn just fine but exit non-zero (`error: could
        // not find Cargo.toml ...`) -- proving the `CommandFailed`, not
        // `Spawn`, path.
        let dir = std::env::temp_dir().join(format!(
            "astrs-xtask-sys-sweep-test-no-manifest-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let err = cargo_tree_stdout(&dir, &["tree"]).unwrap_err();
        assert!(matches!(err, XtaskError::CommandFailed { .. }), "{err:?}");
    }

    // -- The real workspace: the regression guard, mirroring
    // -- `layer_lint`'s `the_real_workspace_has_no_layer_violations` and
    // -- `preflight`'s `the_real_workspace_has_no_oversized_rs_files`.
    // -- `cargo tree` is cheap (graph resolution from the already-committed
    // -- Cargo.lock, no compilation), so -- unlike `clippy`/`nextest` --
    // -- running it for real from a test is in scope here.

    #[test]
    fn the_real_workspace_sys_sweep_is_clean_no_default_features() {
        let root = crate::workspace::workspace_root();
        let report = run(&root, FeatureMode::NoDefaultFeatures).unwrap();
        assert!(report.is_clean(), "disallowed: {:?}", report.disallowed);
    }

    #[test]
    fn the_real_workspace_sys_sweep_is_clean_all_features() {
        let root = crate::workspace::workspace_root();
        let report = run(&root, FeatureMode::AllFeatures).unwrap();
        assert!(report.is_clean(), "disallowed: {:?}", report.disallowed);
    }
}
