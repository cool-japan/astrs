//! `cargo xtask preflight`'s no-inline-version-pins check: every member
//! crate manages its dependencies at the workspace level
//! (~/.claude/CLAUDE.md: *"Workspace: manage deps at workspace level
//! (`*.workspace = true`); no version pins in individual crate
//! Cargo.toml"*), so an inline `version = ".."` (or the equivalent bare
//! `dep = "1.2.3"` shorthand) inside a member's own `[dependencies]`,
//! `[dev-dependencies]` or `[build-dependencies]` table is a policy
//! violation this sweep catches structurally, the same shape as
//! [`crate::layer_lint`] catching an upward dependency edge: pure
//! `Cargo.toml` parsing, no compilation, no subprocess.
//!
//! # What counts as an inline version pin
//!
//! Cargo accepts three shapes for a dependency table entry; this check
//! treats exactly two of them as a pin:
//!
//! - `dep = "1.2.3"` -- a bare string, shorthand for `{ version = "1.2.3"
//!   }`. **A pin.**
//! - `dep = { version = "1.2.3", .. }` -- an inline table carrying an
//!   explicit `version` key (any other keys alongside it -- `features`,
//!   `optional`, `default-features` -- do not change this). **A pin.**
//! - `dep = { workspace = true, .. }` -- inherits both the path and the
//!   version from `[workspace.dependencies]`. **Exempt**, unconditionally:
//!   this is the one shape CLAUDE.md's policy asks for, and every real
//!   dependency line in this workspace already uses it (`grep`-verified:
//!   454 dependency-table entries across every member `Cargo.toml`, all
//!   `workspace = true`, zero exceptions, at the time this check was
//!   written).
//!
//! A path-only entry with neither key (`dep = { path = "../dep" }`) is
//! left alone -- it carries no version to pin, so it is simply outside
//! this specific check's concern, not silently allowed by a gap in it.
//!
//! # The root manifest is exempt by construction, not by a special case
//!
//! [`crate::workspace::discover_member_manifests`] only ever reads
//! `[workspace].members` entries; the root `Cargo.toml`'s own
//! `[workspace.dependencies]` table (where every real `version = ".."` in
//! this workspace lives) is never one of them, so it is never visited by
//! [`check`] at all -- the same structural exemption every other
//! `discover_members`/`discover_member_manifests` caller in this crate
//! (e.g. [`crate::layer_lint::run`]) already relies on for "only ever look
//! at declared members".
//!
//! # Scope boundary: `[target.'cfg(..)'.dependencies]`
//!
//! Not walked -- see [`crate::workspace::MemberManifest`]'s doc comment for
//! why (a pre-existing limitation this check inherits from the manifest
//! model every structural check in this crate already shares, not a new
//! gap; no workspace member currently has such a table).

use std::path::{Path, PathBuf};

use crate::error::XtaskError;
use crate::workspace::{self, MemberManifest};

/// Which dependency table a [`Violation`] was found in. Declared in this
/// order deliberately: deriving [`Ord`] over it this way makes a
/// [`Violation`] sort `[dependencies]` before `[dev-dependencies]` before
/// `[build-dependencies]`, matching the order every real manifest in this
/// workspace already lists them in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DependencyTable {
    /// `[dependencies]`.
    Dependencies,
    /// `[dev-dependencies]`.
    DevDependencies,
    /// `[build-dependencies]`.
    BuildDependencies,
}

impl DependencyTable {
    /// The literal manifest key, e.g. `"dev-dependencies"`.
    const fn manifest_key(self) -> &'static str {
        match self {
            Self::Dependencies => "dependencies",
            Self::DevDependencies => "dev-dependencies",
            Self::BuildDependencies => "build-dependencies",
        }
    }
}

/// Which of the two pinning shapes a [`Violation`] used -- see the module
/// docs' "What counts as an inline version pin" section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinShape {
    /// `dep = "1.2.3"`.
    BareString,
    /// `dep = { version = "1.2.3", .. }`.
    ExplicitVersionKey,
}

impl PinShape {
    const fn description(self) -> &'static str {
        match self {
            Self::BareString => "a bare version-string shorthand",
            Self::ExplicitVersionKey => "an explicit `version` key",
        }
    }
}

/// One dependency entry pinning an inline version instead of inheriting
/// the workspace's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// The offending crate's `[package].name`.
    pub member: String,
    /// That crate's `Cargo.toml`, absolute -- named directly in
    /// [`Display`](std::fmt::Display) so fixing a reported violation never
    /// needs a second lookup from the crate name back to its manifest.
    pub manifest_path: PathBuf,
    /// Which table the entry is in.
    pub table: DependencyTable,
    /// The dependency's key (its name, or its `package` rename target if
    /// one is ever added -- this check only ever looks at the key, never
    /// at a `package` override).
    pub dependency: String,
    /// Which shape pinned it.
    pub shape: PinShape,
    /// The pinned version string itself, when it could be read as one
    /// (always, for a well-formed `Cargo.toml` -- `None` only guards a
    /// malformed `version` value that is not itself a string, which Cargo
    /// would already refuse to build).
    pub pinned_version: Option<String>,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let table = self.table.manifest_key();
        let dep = &self.dependency;
        let version_note = self
            .pinned_version
            .as_ref()
            .map_or_else(String::new, |version| format!(" (\"{version}\")"));
        write!(
            f,
            "{} ({}): [{table}] `{dep}` pins an inline version{version_note} via \
             {} -- use `{dep} = {{ workspace = true }}` instead",
            self.member,
            self.manifest_path.display(),
            self.shape.description(),
        )
    }
}

/// The result of one [`check`]/[`run`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// How many workspace members were checked.
    pub checked: usize,
    /// Every inline-version-pin entry found, sorted by member, then table,
    /// then dependency name (see [`DependencyTable`]'s doc comment for why
    /// that order), so output is deterministic regardless of `toml::Table`
    /// iteration order.
    pub violations: Vec<Violation>,
}

impl Report {
    /// Whether this run found nothing to report.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.violations.is_empty()
    }
}

/// Classify one dependency-table value: [`Some`] with the shape and (when
/// readable) the pinned version string if it is a violation, [`None`] if
/// it is exempt (`{ workspace = true, .. }`) or simply carries no version
/// to pin (`{ path = ".." }` alone, or any non-string/non-table shape --
/// the latter is not valid Cargo dependency syntax to begin with).
fn classify_entry(value: &toml::Value) -> Option<(PinShape, Option<String>)> {
    match value {
        toml::Value::String(version) => Some((PinShape::BareString, Some(version.clone()))),
        toml::Value::Table(table) => {
            if matches!(table.get("workspace"), Some(toml::Value::Boolean(true))) {
                return None;
            }
            let version_value = table.get("version")?;
            let pinned = version_value.as_str().map(str::to_owned);
            Some((PinShape::ExplicitVersionKey, pinned))
        }
        _ => None,
    }
}

/// Check every table of every member in `members`. Pure and
/// filesystem-free -- [`run`] and this module's own tests both funnel
/// through it, mirroring [`crate::layer_lint::lint`]'s split from
/// [`crate::layer_lint::run`].
#[must_use]
pub fn check(members: &[MemberManifest]) -> Report {
    let mut violations = Vec::new();
    for member in members {
        let tables = [
            (DependencyTable::Dependencies, &member.dependencies),
            (DependencyTable::DevDependencies, &member.dev_dependencies),
            (
                DependencyTable::BuildDependencies,
                &member.build_dependencies,
            ),
        ];
        for (table, entries) in tables {
            for (dependency, value) in entries {
                if let Some((shape, pinned_version)) = classify_entry(value) {
                    violations.push(Violation {
                        member: member.name.clone(),
                        manifest_path: member.dir.join("Cargo.toml"),
                        table,
                        dependency: dependency.clone(),
                        shape,
                        pinned_version,
                    });
                }
            }
        }
    }
    violations.sort_by(|a, b| {
        (&a.member, a.table, &a.dependency).cmp(&(&b.member, b.table, &b.dependency))
    });
    Report {
        checked: members.len(),
        violations,
    }
}

/// Discover `root`'s workspace members and [`check`] them.
///
/// # Errors
///
/// Whatever [`workspace::discover_member_manifests`] returns.
pub fn run(root: &Path) -> Result<Report, XtaskError> {
    let members = workspace::discover_member_manifests(root)?;
    Ok(check(&members))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use std::path::PathBuf;

    use super::*;

    /// Parse a `[dependencies]`-table-shaped TOML snippet directly -- the
    /// same "just write real TOML" approach `layer_lint`'s and
    /// `workspace`'s own fixture tests use, but for one table's contents
    /// rather than a whole `Cargo.toml`.
    fn deps(body: &str) -> toml::Table {
        toml::from_str(body).unwrap()
    }

    fn member(name: &str, dependencies: toml::Table) -> MemberManifest {
        MemberManifest {
            name: name.to_owned(),
            dir: PathBuf::from(name),
            dependencies,
            dev_dependencies: toml::Table::new(),
            build_dependencies: toml::Table::new(),
        }
    }

    // -- `classify_entry`: the actual per-entry predicate.

    #[test]
    fn workspace_true_is_exempt() {
        let table = deps(r#"serde = { workspace = true }"#);
        assert_eq!(classify_entry(&table["serde"]), None);
    }

    #[test]
    fn workspace_true_with_extra_keys_is_still_exempt() {
        let table = deps(r#"serde = { workspace = true, features = ["derive"], optional = true }"#);
        assert_eq!(classify_entry(&table["serde"]), None);
    }

    #[test]
    fn a_bare_string_is_a_pin() {
        let table = deps(r#"serde = "1.0.229""#);
        assert_eq!(
            classify_entry(&table["serde"]),
            Some((PinShape::BareString, Some("1.0.229".to_owned())))
        );
    }

    #[test]
    fn an_explicit_version_key_is_a_pin() {
        let table = deps(r#"serde = { version = "1.0.229" }"#);
        assert_eq!(
            classify_entry(&table["serde"]),
            Some((PinShape::ExplicitVersionKey, Some("1.0.229".to_owned())))
        );
    }

    #[test]
    fn an_explicit_version_key_with_extra_keys_is_still_a_pin() {
        let table = deps(r#"serde = { version = "1.0.229", features = ["derive"] }"#);
        assert_eq!(
            classify_entry(&table["serde"]),
            Some((PinShape::ExplicitVersionKey, Some("1.0.229".to_owned())))
        );
    }

    #[test]
    fn a_bare_path_dependency_with_no_version_is_not_a_pin() {
        let table = deps(r#"astrs-wire = { path = "../astrs-wire" }"#);
        assert_eq!(classify_entry(&table["astrs-wire"]), None);
    }

    // -- `check`: the pure per-member/per-table walk.

    #[test]
    fn a_fully_workspace_true_member_is_clean() {
        let members = vec![member(
            "astrs-time",
            deps(
                r#"oxicode = { workspace = true }
serde = { workspace = true }"#,
            ),
        )];
        let report = check(&members);
        assert!(report.is_clean(), "{report:?}");
        assert_eq!(report.checked, 1);
    }

    #[test]
    fn a_bare_string_dependency_is_flagged_precisely() {
        let members = vec![member("astrs-time", deps(r#"serde = "1.0.229""#))];
        let report = check(&members);
        assert_eq!(
            report.violations,
            vec![Violation {
                member: "astrs-time".to_owned(),
                manifest_path: PathBuf::from("astrs-time").join("Cargo.toml"),
                table: DependencyTable::Dependencies,
                dependency: "serde".to_owned(),
                shape: PinShape::BareString,
                pinned_version: Some("1.0.229".to_owned()),
            }]
        );
    }

    #[test]
    fn dev_and_build_dependencies_are_checked_too() {
        let mut m = member("astrs-time", toml::Table::new());
        m.dev_dependencies = deps(r#"proptest = "1.11.0""#);
        m.build_dependencies = deps(r#"cc-helper = { version = "0.1.0" }"#);
        let report = check(&[m]);
        assert_eq!(report.violations.len(), 2);
        assert!(
            report
                .violations
                .iter()
                .any(|v| v.table == DependencyTable::DevDependencies && v.dependency == "proptest")
        );
        assert!(
            report
                .violations
                .iter()
                .any(|v| v.table == DependencyTable::BuildDependencies
                    && v.dependency == "cc-helper")
        );
    }

    #[test]
    fn violations_sort_by_member_then_table_then_dependency() {
        let members = vec![
            member("z-crate", deps(r#"serde = "1.0.0""#)),
            member(
                "a-crate",
                deps(
                    r#"a-dep = "1.0.0"
z-dep = "1.0.0""#,
                ),
            ),
        ];
        let report = check(&members);
        let order: Vec<(&str, &str)> = report
            .violations
            .iter()
            .map(|v| (v.member.as_str(), v.dependency.as_str()))
            .collect();
        assert_eq!(
            order,
            vec![
                ("a-crate", "a-dep"),
                ("a-crate", "z-dep"),
                ("z-crate", "serde")
            ]
        );
    }

    #[test]
    fn display_names_the_member_table_and_pinned_version() {
        let violation = Violation {
            member: "astrs-time".to_owned(),
            manifest_path: PathBuf::from("crates/astrs-time/Cargo.toml"),
            table: DependencyTable::Dependencies,
            dependency: "serde".to_owned(),
            shape: PinShape::BareString,
            pinned_version: Some("1.0.229".to_owned()),
        };
        let text = violation.to_string();
        assert!(text.contains("astrs-time"));
        assert!(text.contains("crates/astrs-time/Cargo.toml"));
        assert!(text.contains("[dependencies]"));
        assert!(text.contains("serde"));
        assert!(text.contains("1.0.229"));
        assert!(text.contains("workspace = true"));
    }

    // -- `run`, against fixture `Cargo.toml`s on disk (`temp_dir`), end to
    // -- end through the real toml parser -- per this check's own brief.

    fn scratch_dir(name: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "astrs-xtask-version-pins-test-{}-{name}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_member(dir: &Path, name: &str, deps_toml: &str) {
        let member_dir = dir.join(name);
        std::fs::create_dir_all(&member_dir).unwrap();
        std::fs::write(
            member_dir.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\n\n{deps_toml}"),
        )
        .unwrap();
    }

    #[test]
    fn run_reads_a_clean_fixture_workspace_from_disk() {
        let dir = scratch_dir("clean");
        std::fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"leaf\"]\n\n\
             [workspace.dependencies]\nserde = { version = \"1.0.229\" }\n",
        )
        .unwrap();
        // The root's OWN `version = ".."` (right above) must never be
        // flagged -- it is never visited as a "member" at all.
        write_member(
            &dir,
            "leaf",
            "[dependencies]\nserde = { workspace = true }\n",
        );

        let report = run(&dir).unwrap();
        assert!(report.is_clean(), "{report:?}");
        assert_eq!(report.checked, 1);
    }

    #[test]
    fn run_reads_a_bare_string_violation_from_disk() {
        let dir = scratch_dir("bare-string");
        std::fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"leaf\"]\n",
        )
        .unwrap();
        write_member(&dir, "leaf", "[dependencies]\nserde = \"1.0.229\"\n");

        let report = run(&dir).unwrap();
        assert_eq!(report.violations.len(), 1);
        assert_eq!(report.violations[0].shape, PinShape::BareString);
    }

    #[test]
    fn run_reads_an_explicit_version_key_violation_from_disk() {
        let dir = scratch_dir("explicit-version");
        std::fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"leaf\"]\n",
        )
        .unwrap();
        write_member(
            &dir,
            "leaf",
            "[dev-dependencies]\nproptest = { version = \"1.11.0\" }\n",
        );

        let report = run(&dir).unwrap();
        assert_eq!(report.violations.len(), 1);
        assert_eq!(report.violations[0].table, DependencyTable::DevDependencies);
        assert_eq!(report.violations[0].shape, PinShape::ExplicitVersionKey);
    }

    // -- The real workspace: the regression guard, mirroring
    // -- `layer_lint`'s `the_real_workspace_has_no_layer_violations`.

    #[test]
    fn the_real_workspace_has_no_inline_version_pins() {
        let root = workspace::workspace_root();
        let report = run(&root).unwrap();
        assert!(
            report.is_clean(),
            "inline version pins found:\n{}",
            report
                .violations
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        );
        assert!(report.checked > 0);
    }
}
