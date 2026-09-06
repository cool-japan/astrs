//! Reading the workspace: its root, its members, and their declared
//! dependencies.
//!
//! [`discover_members`] is the one place in this crate that touches the
//! `toml` crate. It knows just enough of the Cargo manifest shape to answer
//! two questions every other module in this crate needs answered --
//! "what does this crate depend on?" (`layer_lint`) and "in what order can
//! these be published?" (`preflight`'s `--publish-dry-run`) -- without
//! reimplementing `cargo metadata`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::error::XtaskError;

/// The workspace root, derived from this crate's own manifest directory.
///
/// `xtask` is one level below the root (unlike `tests/conformance`, which
/// is two), so a single `.parent()` call finds it -- no search needed, and
/// no dependency on the working directory `cargo xtask` happened to be
/// invoked from.
#[must_use]
pub fn workspace_root() -> PathBuf {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .map_or_else(|| manifest_dir.to_path_buf(), Path::to_path_buf)
}

/// One workspace member's package name, directory, publishability and
/// declared intra-workspace-shaped dependency names.
///
/// Dependency sets hold every key seen in the corresponding Cargo.toml
/// table, external crates (`tokio`, `serde`, ...) included -- callers that
/// only care about `astrs`-prefixed edges (currently every caller) filter
/// at the point of use, rather than this type guessing at their intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// The crate's `[package].name`.
    pub name: String,
    /// The crate's directory, absolute.
    pub dir: PathBuf,
    /// `false` only for an explicit `publish = false`; every other shape
    /// (absent, `true`, or a registry allow-list) is publishable.
    pub publish: bool,
    /// Keys of `[dependencies]`.
    pub dependencies: BTreeSet<String>,
    /// Keys of `[dev-dependencies]`.
    pub dev_dependencies: BTreeSet<String>,
    /// Keys of `[build-dependencies]`.
    pub build_dependencies: BTreeSet<String>,
}

/// The handful of fields this crate reads from the root `Cargo.toml`.
#[derive(Debug, serde::Deserialize)]
struct RootManifest {
    workspace: RootWorkspaceTable,
}

#[derive(Debug, serde::Deserialize)]
struct RootWorkspaceTable {
    members: Vec<String>,
}

/// The handful of fields this crate reads from a member's `Cargo.toml`.
///
/// Every other table (`[lints]`, `[features]`, `[[bin]]`, ...) is simply
/// absent from this struct and ignored by `toml::from_str` -- no
/// `deny_unknown_fields` here, unlike `astrs-manifest`'s own manifest type,
/// because this one is deliberately partial.
#[derive(Debug, serde::Deserialize)]
struct MemberManifestFile {
    package: MemberPackageTable,
    #[serde(default)]
    dependencies: toml::Table,
    #[serde(default, rename = "dev-dependencies")]
    dev_dependencies: toml::Table,
    #[serde(default, rename = "build-dependencies")]
    build_dependencies: toml::Table,
}

#[derive(Debug, serde::Deserialize)]
struct MemberPackageTable {
    name: String,
    /// Cargo accepts `bool` *or* a registry allow-list here; kept as a raw
    /// [`toml::Value`] rather than an enum because the only shape this
    /// crate distinguishes is "is it literally `false`?" (see
    /// [`Member::publish`]'s doc comment) -- everything else, allow-list
    /// included, means publishable.
    #[serde(default)]
    publish: Option<toml::Value>,
}

/// Table keys as an owned, sorted set -- the dependency *names* are all any
/// caller in this crate needs; the version/path/feature shape of the value
/// is not.
fn table_keys(table: toml::Table) -> BTreeSet<String> {
    table.into_iter().map(|(key, _value)| key).collect()
}

/// One workspace member's directory and its three dependency tables with
/// values intact -- unlike [`Member`], which collapses each table to just
/// its key names. [`crate::version_pins`] is the one caller that needs the
/// values themselves, to tell an inline `version = ".."` (or a bare
/// `dep = "1.2.3"` shorthand, equivalent to one) from a `{ workspace =
/// true }` entry; every other module only ever needs to know *that* a
/// dependency edge exists, which [`Member`] already answers more cheaply.
///
/// Like [`Member`] (see its own doc comment), only `[dependencies]`,
/// `[dev-dependencies]` and `[build-dependencies]` are read -- a
/// platform-gated `[target.'cfg(..)'.dependencies]` table is invisible to
/// this struct too, the same pre-existing scope boundary [`Member`] has
/// always had (`MemberManifestFile` below declares no `target` field, so
/// serde silently drops one if a manifest ever has it). No workspace
/// member currently has such a table (verified: `grep -rn
/// '^\[target\.' --include=Cargo.toml` over every member returns
/// nothing), so this is a documented, not a hidden, gap.
#[derive(Debug, Clone)]
pub struct MemberManifest {
    /// The crate's `[package].name`.
    pub name: String,
    /// The crate's directory, absolute.
    pub dir: PathBuf,
    /// The `[dependencies]` table, values intact.
    pub dependencies: toml::Table,
    /// The `[dev-dependencies]` table, values intact.
    pub dev_dependencies: toml::Table,
    /// The `[build-dependencies]` table, values intact.
    pub build_dependencies: toml::Table,
}

/// Parse the root manifest's `[workspace].members` list and every member's
/// own manifest, pairing each with its directory. The one shared reading
/// step behind both [`discover_members`] (which then collapses dependency
/// tables to key names) and [`discover_member_manifests`] (which keeps the
/// values) -- so the two never drift on *how* a member is found, only on
/// what they keep from it.
///
/// # Errors
///
/// [`XtaskError::Io`] if a manifest cannot be read, or
/// [`XtaskError::TomlParse`] if one does not parse as TOML or is missing a
/// field this function needs (`[workspace].members`, `[package].name`).
fn discover_member_manifest_files(
    root: &Path,
) -> Result<Vec<(PathBuf, MemberManifestFile)>, XtaskError> {
    let root_manifest_path = root.join("Cargo.toml");
    let root_manifest: RootManifest = parse_manifest(&root_manifest_path)?;

    let mut out = Vec::with_capacity(root_manifest.workspace.members.len());
    for relative in &root_manifest.workspace.members {
        let dir = root.join(relative);
        let manifest_path = dir.join("Cargo.toml");
        let parsed: MemberManifestFile = parse_manifest(&manifest_path)?;
        out.push((dir, parsed));
    }
    Ok(out)
}

/// Parse `root`'s own `Cargo.toml` and every workspace member's, in the
/// order the root manifest lists them.
///
/// # Errors
///
/// Same as [`discover_member_manifest_files`].
pub fn discover_members(root: &Path) -> Result<Vec<Member>, XtaskError> {
    Ok(discover_member_manifest_files(root)?
        .into_iter()
        .map(|(dir, parsed)| {
            let publish = !matches!(parsed.package.publish, Some(toml::Value::Boolean(false)));
            Member {
                name: parsed.package.name,
                dir,
                publish,
                dependencies: table_keys(parsed.dependencies),
                dev_dependencies: table_keys(parsed.dev_dependencies),
                build_dependencies: table_keys(parsed.build_dependencies),
            }
        })
        .collect())
}

/// Like [`discover_members`], but keeps each dependency table's values
/// rather than collapsing them to key names -- see [`MemberManifest`].
///
/// # Errors
///
/// Same as [`discover_member_manifest_files`].
pub fn discover_member_manifests(root: &Path) -> Result<Vec<MemberManifest>, XtaskError> {
    Ok(discover_member_manifest_files(root)?
        .into_iter()
        .map(|(dir, parsed)| MemberManifest {
            name: parsed.package.name,
            dir,
            dependencies: parsed.dependencies,
            dev_dependencies: parsed.dev_dependencies,
            build_dependencies: parsed.build_dependencies,
        })
        .collect())
}

/// Read and parse one Cargo.toml as `T`.
fn parse_manifest<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, XtaskError> {
    let text = std::fs::read_to_string(path).map_err(|source| XtaskError::io(path, source))?;
    toml::from_str(&text).map_err(|source| XtaskError::toml_parse(path, source))
}

/// A publish-safe topological order of `members`: every crate appears after
/// every other member of `members` that it regular- or build-depends on.
///
/// Dependency edges that name a crate *not* present in `members` (an
/// external crate, or a workspace member the caller filtered out -- e.g.
/// `preflight` restricting to `member.publish`) are ignored: this function
/// only orders the set it was given, on edges internal to that set.
/// `[dev-dependencies]` are ignored entirely -- Cargo does not require a
/// dev-dependency to already be published for `cargo publish` to verify a
/// crate, and this workspace relies on dev-only edges that would otherwise
/// draw a cycle (see `layer_lint`'s module docs for the same exemption
/// applied to layering).
///
/// Ties (crates with no remaining unordered dependency at the same moment)
/// break alphabetically, so the order is deterministic across runs.
///
/// # Errors
///
/// [`XtaskError::CyclicDependencies`] if `members` cannot be fully ordered
/// -- structurally unreachable for `[dependencies]`/`[build-dependencies]`
/// edges alone, since Cargo itself refuses to build a workspace with such a
/// cycle; named explicitly rather than silently returning a partial order.
pub fn topological_order(members: &[Member]) -> Result<Vec<String>, XtaskError> {
    let names: BTreeSet<String> = members.iter().map(|member| member.name.clone()).collect();

    let mut indegree: BTreeMap<String, usize> =
        names.iter().cloned().map(|name| (name, 0)).collect();
    let mut dependents: BTreeMap<String, BTreeSet<String>> = names
        .iter()
        .cloned()
        .map(|name| (name, BTreeSet::new()))
        .collect();

    for member in members {
        let edges = member
            .dependencies
            .iter()
            .chain(member.build_dependencies.iter());
        for dependency in edges {
            if dependency == &member.name || !names.contains(dependency) {
                continue;
            }
            let Some(out_edges) = dependents.get_mut(dependency) else {
                continue;
            };
            if out_edges.insert(member.name.clone())
                && let Some(count) = indegree.get_mut(&member.name)
            {
                *count += 1;
            }
        }
    }

    let mut ready: BTreeSet<String> = indegree
        .iter()
        .filter(|&(_, &count)| count == 0)
        .map(|(name, _)| name.clone())
        .collect();
    let mut order = Vec::with_capacity(members.len());

    while let Some(next) = ready.iter().next().cloned() {
        ready.remove(&next);
        if let Some(unlocked) = dependents.get(&next) {
            for dependent in unlocked.clone() {
                if let Some(count) = indegree.get_mut(&dependent) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        ready.insert(dependent);
                    }
                }
            }
        }
        order.push(next);
    }

    if order.len() == members.len() {
        return Ok(order);
    }
    let ordered: BTreeSet<&String> = order.iter().collect();
    let remaining = names
        .into_iter()
        .filter(|name| !ordered.contains(name))
        .collect();
    Err(XtaskError::CyclicDependencies { remaining })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    /// A fresh, empty temp directory for one test, distinguished by pid,
    /// name and a monotonic counter -- see `astrs-cli`'s `new` command
    /// tests for the same pattern and the race it guards against.
    fn scratch_dir(name: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "astrs-xtask-workspace-test-{}-{name}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_manifest(dir: &Path, member: &str, body: &str) {
        let member_dir = dir.join(member);
        std::fs::create_dir_all(&member_dir).unwrap();
        std::fs::write(member_dir.join("Cargo.toml"), body).unwrap();
    }

    #[test]
    fn discover_members_reads_a_tiny_fixture_workspace() {
        let dir = scratch_dir("tiny");
        std::fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"a\", \"b\"]\n",
        )
        .unwrap();
        write_manifest(&dir, "a", "[package]\nname = \"a\"\n\n[dependencies]\n");
        write_manifest(
            &dir,
            "b",
            "[package]\nname = \"b\"\npublish = false\n\n\
             [dependencies]\na = { path = \"../a\" }\ntokio = \"1\"\n\n\
             [dev-dependencies]\nc = { path = \"../c\" }\n",
        );

        let members = discover_members(&dir).unwrap();
        assert_eq!(members.len(), 2);

        let a = members.iter().find(|m| m.name == "a").unwrap();
        assert!(a.publish);
        assert!(a.dependencies.is_empty());

        let b = members.iter().find(|m| m.name == "b").unwrap();
        assert!(!b.publish, "publish = false must be honored");
        assert_eq!(
            b.dependencies,
            BTreeSet::from(["a".to_owned(), "tokio".to_owned()])
        );
        assert_eq!(b.dev_dependencies, BTreeSet::from(["c".to_owned()]));
        assert!(b.build_dependencies.is_empty());
    }

    #[test]
    fn publish_true_and_absent_and_registry_list_are_all_publishable() {
        let dir = scratch_dir("publishable-shapes");
        std::fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"absent\", \"explicit-true\", \"registry-list\"]\n",
        )
        .unwrap();
        write_manifest(&dir, "absent", "[package]\nname = \"absent\"\n");
        write_manifest(
            &dir,
            "explicit-true",
            "[package]\nname = \"explicit-true\"\npublish = true\n",
        );
        write_manifest(
            &dir,
            "registry-list",
            "[package]\nname = \"registry-list\"\npublish = [\"my-registry\"]\n",
        );

        let members = discover_members(&dir).unwrap();
        assert!(members.iter().all(|m| m.publish), "{members:?}");
    }

    #[test]
    fn missing_root_manifest_is_an_io_error() {
        let dir = scratch_dir("missing-root");
        let err = discover_members(&dir).unwrap_err();
        assert!(matches!(err, XtaskError::Io { .. }));
    }

    #[test]
    fn a_member_manifest_missing_the_package_table_is_a_parse_error() {
        let dir = scratch_dir("missing-package");
        std::fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"broken\"]\n",
        )
        .unwrap();
        write_manifest(&dir, "broken", "# no [package] table at all\n");
        let err = discover_members(&dir).unwrap_err();
        assert!(matches!(err, XtaskError::TomlParse { .. }));
    }

    fn member(name: &str, deps: &[&str]) -> Member {
        Member {
            name: name.to_owned(),
            dir: PathBuf::from(name),
            publish: true,
            dependencies: deps.iter().map(|s| (*s).to_owned()).collect(),
            dev_dependencies: BTreeSet::new(),
            build_dependencies: BTreeSet::new(),
        }
    }

    #[test]
    fn topological_order_orders_a_diamond() {
        // base <- {left, right} <- top
        let members = vec![
            member("top", &["left", "right"]),
            member("left", &["base"]),
            member("right", &["base"]),
            member("base", &[]),
        ];
        let order = topological_order(&members).unwrap();
        assert_eq!(order.len(), 4);
        let position = |name: &str| order.iter().position(|n| n == name).unwrap();
        assert!(position("base") < position("left"));
        assert!(position("base") < position("right"));
        assert!(position("left") < position("top"));
        assert!(position("right") < position("top"));
    }

    #[test]
    fn topological_order_breaks_ties_alphabetically() {
        let members = vec![member("b", &[]), member("a", &[]), member("c", &[])];
        assert_eq!(topological_order(&members).unwrap(), vec!["a", "b", "c"]);
    }

    #[test]
    fn topological_order_ignores_edges_outside_the_given_set() {
        // `member` regular-depends on a crate not present in `members` at
        // all (e.g. `preflight` restricted to publishable crates) -- must
        // not be treated as an unsatisfiable dependency.
        let members = vec![member("solo", &["not-in-this-set"])];
        assert_eq!(topological_order(&members).unwrap(), vec!["solo"]);
    }

    #[test]
    fn topological_order_ignores_self_dependencies() {
        // astrs-operator-macros' real Cargo.toml dev-depends on itself;
        // regular self-edges must not deadlock the algorithm either.
        let members = vec![member("self-loop", &["self-loop"])];
        assert_eq!(topological_order(&members).unwrap(), vec!["self-loop"]);
    }

    #[test]
    fn topological_order_reports_a_genuine_cycle() {
        let members = vec![member("x", &["y"]), member("y", &["x"])];
        let err = topological_order(&members).unwrap_err();
        match err {
            XtaskError::CyclicDependencies { remaining } => {
                assert_eq!(remaining, vec!["x".to_owned(), "y".to_owned()]);
            }
            other => panic!("expected CyclicDependencies, got {other:?}"),
        }
    }

    #[test]
    fn workspace_root_holds_the_real_root_cargo_toml() {
        let root = workspace_root();
        assert!(root.join("Cargo.toml").is_file(), "{}", root.display());
        assert!(
            root.join("xtask/Cargo.toml").is_file(),
            "{}",
            root.display()
        );
    }
}
