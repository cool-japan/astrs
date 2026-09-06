//! The local clone of the AstRS package index (blueprint §22's "hub/package
//! index"), and resolution of a manifest [`HubSource`] against it.
//!
//! # The index repo's layout
//!
//! A minimal, self-hostable git repository:
//!
//! ```text
//!   <index repo>/
//!     packages/
//!       <name>.json   # one file per package
//! ```
//!
//! Each `packages/<name>.json` is one [`IndexPackage`]:
//!
//! ```json
//! {
//!   "name": "yolo-detector",
//!   "description": "A YOLO-family object detector node.",
//!   "versions": [
//!     { "version": "v0.3.0", "git": "https://example.invalid/yolo", "rev": "a1b2c3d" },
//!     { "version": "v0.3.1", "git": "https://example.invalid/yolo", "rev": "d4e5f6a" }
//!   ]
//! }
//! ```
//!
//! `versions` is append-only, oldest first — exactly how a real registry's
//! publish log grows, and how `astrs hub init`'s scaffold and `astrs hub`'s
//! own README document it. "Latest" ([`HubSource::rev`] absent) is therefore
//! the **last** element, never the numerically-largest `version` string —
//! this crate does no semver comparison, which keeps a package free to use
//! whatever versioning scheme it likes (see `select_version`, this
//! module's own private version-selection helper).
//!
//! A version entry's own `rev` is the exact git commit/tag/branch its
//! `version` name resolves to — always present, because a published version
//! is a promise of reproducibility a floating ref cannot keep. `subdir` is
//! the directory this package lives in *within* the cloned repository, for
//! a repo that hosts more than one package (monorepo-style) — a package
//! whose whole repo *is* the package leaves it unset, exactly as a `git:`
//! source with no `path:` clones without needing one.
//!
//! # This module only reads what is already on disk
//!
//! Fetching the index repo itself — the `git clone`/`git pull` that
//! populates [`cache_dir_from_env`]'s directory — is `astrs hub update`'s
//! job, in `astrs-cli`, not this module's: a manifest submitted to a
//! standalone `astrs coordinator` process over the wire (the ordinary
//! `astrs up`/`astrs start` path, `crate::handlers::lifecycle`) is resolved
//! there, with no CLI anywhere nearby, so [`resolve`] and
//! [`resolve_from_env`] never reach across the network — they read files,
//! or fail with [`HubIndexError::IndexMissing`] naming the update command
//! that would fix it. See `crate::graph_bridge` for where a resolved
//! package lowers onto the same `git clone`/`checkout` machinery an
//! ordinary `git:` source uses.
//!
//! # Which process's cache is authoritative
//!
//! `crate::graph_bridge::node_source_for` — this module's other caller —
//! also backs `expand_node`/`expand_node_fragment`, which `astrs-cli`'s
//! `astrs node add`/`astrs node replace` (blueprint §17) run *client-side*,
//! in the CLI's own process, before ever dialing the coordinator (see
//! those functions' own docs). A `hub:` source on a dynamically-added node
//! therefore resolves against whichever machine ran `astrs node add` —
//! not necessarily the same machine, or the same freshly-`astrs hub
//! update`d cache, as the one running `astrs coordinator`. On the common
//! single-machine development workflow the two are the same process's
//! environment and this is invisible; across machines, an unpinned `hub:
//! pkg` (no `@rev`) can resolve to whatever each side's cache last pulled
//! as "latest". This is the same shared-nothing-between-processes shape
//! `git:` sources already have (each side resolves `node.git`/`node.rev`
//! independently), made visible here only because "latest" is now a
//! question the *cache's contents* answer rather than the manifest text
//! alone.

use std::path::{Path, PathBuf};

use astrs_manifest::HubSource;
use serde::{Deserialize, Serialize};

use crate::error::CoordinatorError;

/// The environment variable that overrides the local index clone's
/// location, honored by both `astrs hub update` (which populates it) and
/// [`cache_dir_from_env`] (which every hub-source resolution reads from) —
/// see [`crate::hub_index`]'s module docs on why nothing else may compute
/// this path independently.
pub const ENV_HUB_CACHE_DIR: &str = "ASTRS_HUB_CACHE_DIR";

/// The `XDG_CACHE_HOME`-style variable [`cache_dir_from_env`] consults
/// before falling back to `$HOME`.
const ENV_XDG_CACHE_HOME: &str = "XDG_CACHE_HOME";

/// The path segments appended under whichever cache root is chosen.
const CACHE_SUBDIR: [&str; 2] = ["astrs", "hub-index"];

/// The default index repository — a `astrs hub update` with no
/// `--index`/[`ENV_HUB_INDEX`]/config-file override clones this. It may not
/// exist yet (blueprint §22 is a 0.2.0 roadmap item this release pulls
/// forward): every path that reaches out to it degrades to a clear,
/// actionable error rather than an opaque network failure — see
/// `astrs-cli`'s `command::hub` for where that happens.
pub const DEFAULT_INDEX_URL: &str = "https://github.com/cool-japan/astrs-hub";

/// The environment variable naming an explicit index repository URL,
/// checked before the config-file key of the same purpose (both ahead of
/// [`DEFAULT_INDEX_URL`]) — see `astrs-cli`'s `command::hub::resolve_index_url`.
pub const ENV_HUB_INDEX: &str = "ASTRS_HUB_INDEX";

/// The relative path from an index repo's root to one package's entry.
const PACKAGES_DIR: &str = "packages";

/// One published version of a hub package — one element of
/// [`IndexPackage::versions`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexVersion {
    /// The human-facing version selector (`"v0.3.1"`, `"2024-01-09"`,
    /// whatever the publisher chooses — never compared as semver by this
    /// crate; see the module docs).
    pub version: String,
    /// The git repository this version's package lives in.
    pub git: String,
    /// The exact commit, tag or branch `version` names — always present
    /// (a published version pins something reproducible; see the module
    /// docs for why this differs from [`HubSource::rev`], which may be
    /// absent).
    pub rev: String,
    /// This package's directory within the repository, for a repo hosting
    /// more than one package. Absent means the repository root — the same
    /// convention an ordinary `git:` source with no `path:` uses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdir: Option<String>,
}

/// One `packages/<name>.json` entry: a package's identity and every
/// version it has published.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexPackage {
    /// The package's name — the index's own charset check on this is
    /// [`HubSource::is_valid_name`] (checked by [`read_package`] before it
    /// ever opens a file, not re-derived here).
    pub name: String,
    /// A one-line, human-readable summary, shown by `astrs hub search`/
    /// `astrs hub info`.
    pub description: String,
    /// Every published version, oldest first (see the module docs on why
    /// "latest" is the last element rather than a semver comparison).
    pub versions: Vec<IndexVersion>,
}

/// A [`HubSource`] resolved against the local index: the git repository,
/// exact revision and (optional) in-repo subdirectory a `hub:` node lowers
/// onto — see `crate::graph_bridge` for the lowering itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedHubPackage {
    /// The resolved package's name (identical to [`HubSource::name`];
    /// carried here so a caller holding only the resolved value can still
    /// name the package in a message).
    pub name: String,
    /// The version actually resolved — the requested one, or the latest
    /// when [`HubSource::rev`] was absent.
    pub version: String,
    /// The git repository to clone.
    pub git: String,
    /// The exact commit/tag/branch to check out.
    pub rev: String,
    /// The package's directory within that repository, when it is not the
    /// repository root.
    pub subdir: Option<String>,
}

/// Everything that can go wrong resolving a `hub:` source against the
/// local index, each variant worded as the actionable message
/// [`crate::CoordinatorError::InvalidArgument`] surfaces to whoever
/// submitted the manifest (a CLI user, or a coordinator client) — see this
/// module's `impl From<HubIndexError> for CoordinatorError` below.
#[derive(Debug, thiserror::Error)]
pub enum HubIndexError {
    /// The local index clone (or its `packages/` directory) does not exist
    /// at all.
    #[error(
        "no local package index at `{path}` (or it has no `packages/` directory yet); run \
         `astrs hub update` first"
    )]
    IndexMissing {
        /// The index root that was checked.
        path: String,
    },
    /// A package name failed [`HubSource::is_valid_name`] — checked before
    /// any path is built from it, so a name that is not `[a-z0-9-]+` can
    /// never reach the filesystem (defense against `..`/`/` path
    /// traversal through an unvalidated `astrs hub info <name>` argument).
    #[error(
        "`{name}` is not a valid hub package name: names must be non-empty and match [a-z0-9-]+"
    )]
    InvalidName {
        /// The rejected name, verbatim.
        name: String,
    },
    /// The index exists but has no entry for this package.
    #[error(
        "no package named `{name}` in the local package index (looked for `{path}`); try \
         `astrs hub search {name}`, or `astrs hub update` to refresh the index"
    )]
    PackageNotFound {
        /// The package name that was looked up.
        name: String,
        /// The exact file that was missing.
        path: String,
    },
    /// The package's `packages/<name>.json` entry exists but is not valid
    /// JSON, or does not match [`IndexPackage`]'s shape.
    #[error("`{path}` is not a valid package index entry: {source}")]
    Malformed {
        /// The package name that was looked up.
        name: String,
        /// The malformed file's path.
        path: String,
        /// The underlying parse error.
        #[source]
        source: serde_json::Error,
    },
    /// The package's entry parses, but declares no versions at all.
    #[error("package `{name}` has no published versions")]
    NoVersions {
        /// The package with no versions.
        name: String,
    },
    /// [`HubSource::rev`] named a version this package has not published.
    #[error("package `{name}` has no published version `{requested}` (known versions: {known})")]
    UnknownVersion {
        /// The package that was found.
        name: String,
        /// The version string that did not match any published one.
        requested: String,
        /// Every version this package *has* published, comma-joined, for
        /// the error message.
        known: String,
    },
    /// A filesystem operation on the index failed for a reason other than
    /// "not found" (permissions, a directory where a file was expected,
    /// …).
    #[error("failed to read `{path}`: {source}")]
    Io {
        /// The path the failing operation targeted.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

impl From<HubIndexError> for CoordinatorError {
    fn from(error: HubIndexError) -> Self {
        Self::invalid(error.to_string())
    }
}

/// The local index clone's directory, honoring [`ENV_HUB_CACHE_DIR`] first.
///
/// Falls back to `$XDG_CACHE_HOME/astrs/hub-index`, then
/// `$HOME/.cache/astrs/hub-index`, then a fixed name under
/// [`std::env::temp_dir`] — the same explicit-override-then-XDG-then-degrade
/// shape `astrs_daemon::config::paths::default_runtime_dir` uses for the
/// runtime directory, so the convention is familiar even though the two
/// crates cannot share the function itself (`astrs-coordinator` does not
/// depend on `astrs-daemon`).
///
/// `astrs hub update` must clone/pull into exactly this directory — see the
/// module docs — which is why this function lives here rather than being
/// re-derived in `astrs-cli`.
#[must_use]
pub fn cache_dir_from_env() -> PathBuf {
    if let Some(explicit) = non_empty_var(ENV_HUB_CACHE_DIR) {
        return PathBuf::from(explicit);
    }
    if let Some(xdg) = non_empty_var(ENV_XDG_CACHE_HOME) {
        let mut dir = PathBuf::from(xdg);
        dir.extend(CACHE_SUBDIR);
        return dir;
    }
    if let Some(home) = non_empty_var("HOME") {
        let mut dir = PathBuf::from(home);
        dir.push(".cache");
        dir.extend(CACHE_SUBDIR);
        return dir;
    }
    let mut dir = std::env::temp_dir();
    dir.push("astrs-hub-index");
    dir
}

/// Reads an environment variable, treating an empty value as unset —
/// mirrors `astrs_daemon::config::paths`' own `non_empty_var`.
fn non_empty_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// The `packages/` directory under an index root.
fn packages_dir(index_root: &Path) -> PathBuf {
    index_root.join(PACKAGES_DIR)
}

/// The file one package's entry lives at, under an index root.
fn package_path(index_root: &Path, name: &str) -> PathBuf {
    packages_dir(index_root).join(format!("{name}.json"))
}

/// Reads and parses one package's index entry.
///
/// Used both by [`resolve`] and directly by `astrs hub info` (which wants
/// the whole [`IndexPackage`], not one resolved version).
///
/// # Errors
///
/// - [`HubIndexError::InvalidName`] if `name` fails
///   [`HubSource::is_valid_name`] — checked first, before any path is
///   built from `name`, so a name outside the `[a-z0-9-]+` charset can
///   never be used to read a file outside `packages/`.
/// - [`HubIndexError::IndexMissing`] if `index_root` has no `packages/`
///   directory.
/// - [`HubIndexError::PackageNotFound`] if `packages/<name>.json` does not
///   exist.
/// - [`HubIndexError::Malformed`] if it exists but is not a valid
///   [`IndexPackage`].
/// - [`HubIndexError::Io`] for any other filesystem failure.
pub fn read_package(index_root: &Path, name: &str) -> Result<IndexPackage, HubIndexError> {
    if !HubSource::is_valid_name(name) {
        return Err(HubIndexError::InvalidName {
            name: name.to_owned(),
        });
    }
    if !packages_dir(index_root).is_dir() {
        return Err(HubIndexError::IndexMissing {
            path: index_root.display().to_string(),
        });
    }
    let path = package_path(index_root, name);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(HubIndexError::PackageNotFound {
                name: name.to_owned(),
                path: path.display().to_string(),
            });
        }
        Err(source) => {
            return Err(HubIndexError::Io {
                path: path.display().to_string(),
                source,
            });
        }
    };
    serde_json::from_str(&text).map_err(|source| HubIndexError::Malformed {
        name: name.to_owned(),
        path: path.display().to_string(),
        source,
    })
}

/// Picks the version a [`HubSource`] names out of a package's published
/// list: an exact match on [`HubSource::rev`] when it names one, else the
/// last (newest) entry — see the module docs on why this is a straight
/// string match, never a semver comparison or a fallback to an arbitrary
/// git ref.
fn select_version<'a>(
    package: &'a IndexPackage,
    requested: Option<&str>,
) -> Result<&'a IndexVersion, HubIndexError> {
    match requested {
        None => package
            .versions
            .last()
            .ok_or_else(|| HubIndexError::NoVersions {
                name: package.name.clone(),
            }),
        Some(requested) => {
            if package.versions.is_empty() {
                return Err(HubIndexError::NoVersions {
                    name: package.name.clone(),
                });
            }
            package
                .versions
                .iter()
                .find(|candidate| candidate.version == requested)
                .ok_or_else(|| HubIndexError::UnknownVersion {
                    name: package.name.clone(),
                    requested: requested.to_owned(),
                    known: package
                        .versions
                        .iter()
                        .map(|version| version.version.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                })
        }
    }
}

/// Resolves a manifest `hub:` source against the local index clone at
/// `index_root`: looks up [`HubSource::name`], then picks the version
/// [`HubSource::rev`] names (or the latest, when absent) — see
/// `select_version`, this module's own private version-selection helper.
///
/// The tested, explicit-path core: [`resolve_from_env`] is the thin
/// environment-reading wrapper every real caller uses, so a test can drive
/// this function against a hand-built index in `std::env::temp_dir()`
/// without touching the process environment at all.
///
/// # Errors
///
/// As [`read_package`], plus [`HubIndexError::NoVersions`] /
/// [`HubIndexError::UnknownVersion`] from that same selection step.
pub fn resolve(index_root: &Path, source: &HubSource) -> Result<ResolvedHubPackage, HubIndexError> {
    let package = read_package(index_root, &source.name)?;
    let version = select_version(&package, source.rev.as_deref())?;
    // Cloned into owned locals so `version`'s borrow of `package` ends
    // before `package.name` moves into the result below.
    let (resolved_version, git, rev, subdir) = (
        version.version.clone(),
        version.git.clone(),
        version.rev.clone(),
        version.subdir.clone(),
    );
    Ok(ResolvedHubPackage {
        name: package.name,
        version: resolved_version,
        git,
        rev,
        subdir,
    })
}

/// As [`resolve`], against [`cache_dir_from_env`]'s directory — what
/// `crate::graph_bridge` calls when lowering a `hub:`-sourced node.
///
/// # Errors
///
/// As [`resolve`].
pub fn resolve_from_env(source: &HubSource) -> Result<ResolvedHubPackage, HubIndexError> {
    resolve(&cache_dir_from_env(), source)
}

/// Every package in the local index whose name or description contains
/// `term` (case-insensitively; an empty `term` matches everything),
/// sorted by name.
///
/// An entry that fails to parse is skipped rather than failing the whole
/// search — one publisher's malformed `packages/<name>.json` should not
/// stop every other package from being found; `astrs hub info` on the
/// specific name is what surfaces a [`HubIndexError::Malformed`] in full.
///
/// # Errors
///
/// [`HubIndexError::IndexMissing`] if `index_root` has no `packages/`
/// directory. [`HubIndexError::Io`] if that directory exists but cannot be
/// listed.
pub fn search(index_root: &Path, term: &str) -> Result<Vec<IndexPackage>, HubIndexError> {
    let dir = packages_dir(index_root);
    if !dir.is_dir() {
        return Err(HubIndexError::IndexMissing {
            path: index_root.display().to_string(),
        });
    }
    let needle = term.to_ascii_lowercase();
    let entries = std::fs::read_dir(&dir).map_err(|source| HubIndexError::Io {
        path: dir.display().to_string(),
        source,
    })?;

    let mut hits = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().and_then(std::ffi::OsStr::to_str) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(package) = serde_json::from_str::<IndexPackage>(&text) else {
            continue;
        };
        if needle.is_empty()
            || package.name.to_ascii_lowercase().contains(&needle)
            || package.description.to_ascii_lowercase().contains(&needle)
        {
            hits.push(package);
        }
    }
    hits.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(hits)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "astrs-coordinator-hub-index-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(PACKAGES_DIR)).unwrap();
        dir
    }

    fn write_package(index_root: &Path, package: &IndexPackage) {
        let text = serde_json::to_string_pretty(package).unwrap();
        std::fs::write(package_path(index_root, &package.name), text).unwrap();
    }

    fn sample_package() -> IndexPackage {
        IndexPackage {
            name: "yolo-detector".to_owned(),
            description: "A YOLO-family object detector node.".to_owned(),
            versions: vec![
                IndexVersion {
                    version: "v0.3.0".to_owned(),
                    git: "https://example.invalid/yolo".to_owned(),
                    rev: "a1b2c3d".to_owned(),
                    subdir: None,
                },
                IndexVersion {
                    version: "v0.3.1".to_owned(),
                    git: "https://example.invalid/yolo".to_owned(),
                    rev: "d4e5f6a".to_owned(),
                    subdir: Some("nodes/yolo".to_owned()),
                },
            ],
        }
    }

    #[test]
    fn resolving_with_no_rev_picks_the_last_entry_as_latest() {
        let dir = scratch("latest");
        write_package(&dir, &sample_package());

        let resolved = resolve(&dir, &HubSource::latest("yolo-detector")).unwrap();
        assert_eq!(resolved.version, "v0.3.1");
        assert_eq!(resolved.rev, "d4e5f6a");
        assert_eq!(resolved.subdir.as_deref(), Some("nodes/yolo"));
    }

    #[test]
    fn resolving_with_a_pinned_rev_matches_that_exact_version() {
        let dir = scratch("pinned");
        write_package(&dir, &sample_package());

        let resolved = resolve(&dir, &HubSource::pinned("yolo-detector", "v0.3.0")).unwrap();
        assert_eq!(resolved.version, "v0.3.0");
        assert_eq!(resolved.rev, "a1b2c3d");
        assert_eq!(resolved.subdir, None);
    }

    #[test]
    fn an_unpublished_version_is_a_clear_bad_rev_error() {
        let dir = scratch("bad-rev");
        write_package(&dir, &sample_package());

        let error = resolve(&dir, &HubSource::pinned("yolo-detector", "v9.9.9")).unwrap_err();
        assert!(
            matches!(error, HubIndexError::UnknownVersion { .. }),
            "{error}"
        );
        let message = error.to_string();
        assert!(message.contains("v9.9.9"), "{message}");
        assert!(message.contains("v0.3.0"), "{message}");
        assert!(message.contains("v0.3.1"), "{message}");
    }

    #[test]
    fn an_unknown_package_names_the_search_and_update_commands() {
        let dir = scratch("unknown-package");
        // The index itself exists (packages/ is there), just not this
        // package — distinct from the index being entirely absent.
        let error = resolve(&dir, &HubSource::latest("does-not-exist")).unwrap_err();
        assert!(
            matches!(error, HubIndexError::PackageNotFound { .. }),
            "{error}"
        );
        let message = error.to_string();
        assert!(message.contains("astrs hub search"), "{message}");
        assert!(message.contains("astrs hub update"), "{message}");
    }

    #[test]
    fn a_missing_index_names_the_update_command() {
        let dir = std::env::temp_dir().join(format!(
            "astrs-coordinator-hub-index-{}-never-created",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let error = resolve(&dir, &HubSource::latest("anything")).unwrap_err();
        assert!(
            matches!(error, HubIndexError::IndexMissing { .. }),
            "{error}"
        );
        assert!(error.to_string().contains("astrs hub update"));
    }

    #[test]
    fn malformed_json_is_reported_rather_than_treated_as_not_found() {
        let dir = scratch("malformed");
        std::fs::write(package_path(&dir, "broken"), "{ not json").unwrap();

        let error = resolve(&dir, &HubSource::latest("broken")).unwrap_err();
        assert!(matches!(error, HubIndexError::Malformed { .. }), "{error}");
    }

    #[test]
    fn a_package_with_no_versions_is_reported_distinctly() {
        let dir = scratch("no-versions");
        write_package(
            &dir,
            &IndexPackage {
                name: "empty-pkg".to_owned(),
                description: "nothing published yet".to_owned(),
                versions: Vec::new(),
            },
        );

        let error = resolve(&dir, &HubSource::latest("empty-pkg")).unwrap_err();
        assert!(matches!(error, HubIndexError::NoVersions { .. }), "{error}");
    }

    #[test]
    fn a_name_outside_the_charset_never_touches_the_filesystem() {
        let dir = scratch("traversal");
        // No file is created for this "name" at all — if `read_package`
        // built a path from it before validating, this would still (on
        // this platform) fail to escape `packages/`, but the point is that
        // it must be rejected *before* any path or file I/O is attempted.
        let error = resolve(&dir, &HubSource::latest("../../etc/passwd")).unwrap_err();
        assert!(
            matches!(error, HubIndexError::InvalidName { .. }),
            "{error}"
        );
    }

    #[test]
    fn search_matches_name_or_description_case_insensitively() {
        let dir = scratch("search");
        write_package(&dir, &sample_package());
        write_package(
            &dir,
            &IndexPackage {
                name: "lidar-slam".to_owned(),
                description: "SLAM from a spinning lidar.".to_owned(),
                versions: vec![IndexVersion {
                    version: "v1".to_owned(),
                    git: "https://example.invalid/slam".to_owned(),
                    rev: "abc".to_owned(),
                    subdir: None,
                }],
            },
        );

        let by_name = search(&dir, "YOLO").unwrap();
        assert_eq!(by_name.len(), 1);
        assert_eq!(by_name[0].name, "yolo-detector");

        let by_description = search(&dir, "spinning").unwrap();
        assert_eq!(by_description.len(), 1);
        assert_eq!(by_description[0].name, "lidar-slam");

        let everything = search(&dir, "").unwrap();
        assert_eq!(everything.len(), 2);
        // Sorted by name: "lidar-slam" < "yolo-detector".
        assert_eq!(everything[0].name, "lidar-slam");

        let nothing = search(&dir, "nonexistent-term").unwrap();
        assert!(nothing.is_empty());
    }

    #[test]
    fn search_skips_a_malformed_entry_rather_than_failing_outright() {
        let dir = scratch("search-malformed");
        write_package(&dir, &sample_package());
        std::fs::write(package_path(&dir, "broken"), "{ not json").unwrap();

        let hits = search(&dir, "").unwrap();
        assert_eq!(hits.len(), 1, "the malformed entry is skipped, not fatal");
        assert_eq!(hits[0].name, "yolo-detector");
    }

    #[test]
    fn search_against_a_missing_index_is_a_clear_error() {
        let dir = std::env::temp_dir().join(format!(
            "astrs-coordinator-hub-index-{}-search-missing",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let error = search(&dir, "anything").unwrap_err();
        assert!(
            matches!(error, HubIndexError::IndexMissing { .. }),
            "{error}"
        );
    }

    #[test]
    fn read_package_returns_the_whole_entry_for_hub_info() {
        let dir = scratch("read-package");
        write_package(&dir, &sample_package());

        let package = read_package(&dir, "yolo-detector").unwrap();
        assert_eq!(package.versions.len(), 2);
        assert_eq!(package.description, "A YOLO-family object detector node.");
    }

    #[test]
    fn cache_dir_from_env_honors_the_explicit_override() {
        // SAFETY-equivalent: `std::env::set_var` is not literally `unsafe`
        // here (nextest runs each test in its own process), but tests
        // sharing an env var must not run concurrently with each other;
        // this test owns a variable name no other test in this crate
        // touches.
        let explicit = std::env::temp_dir().join("astrs-hub-cache-dir-override-test");
        unsafe {
            std::env::set_var(ENV_HUB_CACHE_DIR, &explicit);
        }
        let resolved = cache_dir_from_env();
        unsafe {
            std::env::remove_var(ENV_HUB_CACHE_DIR);
        }
        assert_eq!(resolved, explicit);
    }

    #[test]
    fn hub_index_error_converts_into_a_coordinator_error() {
        let error: CoordinatorError = HubIndexError::NoVersions {
            name: "x".to_owned(),
        }
        .into();
        assert!(error.to_string().contains("no published versions"));
    }
}
