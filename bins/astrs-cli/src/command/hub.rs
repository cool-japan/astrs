//! `astrs hub` — the local client for the AstRS package index (blueprint
//! §22's "hub/package index"), pulled forward from the 0.2.0 roadmap into
//! this release.
//!
//! ```text
//!   astrs hub init <dir>     scaffold a self-hostable index repo layout
//!   astrs hub update         clone/pull the index repo into the local cache
//!   astrs hub search <term>  list packages whose name/description match
//!   astrs hub info <name>    show one package's published versions
//! ```
//!
//! # What this module does *not* do
//!
//! Resolving a manifest's `hub:` source into a `git clone`/`checkout` pair
//! — reading the very index this module manages — is
//! `astrs_coordinator::hub_index`'s job, called from
//! `astrs_coordinator::graph_bridge` at dataflow-start time, not from here:
//! a standalone `astrs coordinator` process (never running any CLI verb at
//! all) resolves `hub:` sources for manifests submitted to it over the
//! wire, so the resolution logic cannot live behind a CLI-only module. This
//! module and that one agree on exactly one thing —
//! [`astrs_coordinator::hub_index::cache_dir_from_env`], the local index
//! clone's location — which is why `update` clones/pulls into precisely
//! that directory rather than computing its own.
//!
//! # Index URL resolution
//!
//! [`resolve_index_url`]: an explicit `--index` argument, then
//! [`astrs_coordinator::hub_index::ENV_HUB_INDEX`], then the `[hub] index`
//! key of the config file at [`default_config_path`], then
//! [`astrs_coordinator::hub_index::DEFAULT_INDEX_URL`] — which may not
//! exist yet (this is a roadmap item pulled forward), so `update` against
//! it fails with the system `git` binary's own clear "repository not
//! found" message rather than anything this module invents.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use astrs_coordinator::CoordinatorError;
use astrs_coordinator::hub_index::{self, IndexPackage};
use serde::Deserialize;

use crate::error::CliError;

/// The file name a `[hub]` section is read from, under [`default_config_path`].
pub const CONFIG_FILE_NAME: &str = "config.toml";

/// The `README.md` an `astrs hub init` scaffold gets, explaining the
/// layout well enough that a self-hoster does not need to re-read this
/// module's docs.
const INDEX_README: &str = "\
# AstRS package index

A minimal, git-hosted index for `hub:` manifest sources (blueprint §22).

## Layout

    packages/<name>.json

Each file is one package:

```json
{
  \"name\": \"yolo-detector\",
  \"description\": \"A YOLO-family object detector node.\",
  \"versions\": [
    { \"version\": \"v0.3.0\", \"git\": \"https://example.invalid/yolo\", \"rev\": \"a1b2c3d\" },
    { \"version\": \"v0.3.1\", \"git\": \"https://example.invalid/yolo\", \"rev\": \"d4e5f6a\", \"subdir\": \"nodes/yolo\" }
  ]
}
```

`versions` is append-only, oldest first: publishing a new version means
appending an entry, never rewriting one already there. `hub: <name>` with
no `@rev` resolves to the last (newest) entry; `hub: <name>@<version>`
must match a `version` string exactly.

`rev` is the exact git commit/tag/branch that `version` resolves to —
always required, so a published version stays reproducible even if a
branch it once pointed at moves on. `subdir` is optional: the package's
own directory within `git`, for a repository that hosts more than one
package. Leave it out when the whole repository *is* the package.

## Publishing

Add or edit a `packages/<name>.json` file, commit, and push. Anyone
running `astrs hub update` against this repository's URL picks up the
change on their next update.
";

/// Where `astrs hub`'s config file lives, honoring `XDG_CONFIG_HOME` first.
///
/// Falls back to `$HOME/.config/astrs/config.toml`, then a fixed name
/// under [`std::env::temp_dir`] — the same shape
/// [`hub_index::cache_dir_from_env`] uses for the index cache itself.
#[must_use]
pub fn default_config_path() -> PathBuf {
    if let Some(xdg) = non_empty_var("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("astrs").join(CONFIG_FILE_NAME);
    }
    if let Some(home) = non_empty_var("HOME") {
        return PathBuf::from(home)
            .join(".config")
            .join("astrs")
            .join(CONFIG_FILE_NAME);
    }
    std::env::temp_dir().join(format!("astrs-{CONFIG_FILE_NAME}"))
}

fn non_empty_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// The `[hub]` section of `astrs`'s config file — the only section this
/// release reads or writes.
#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    hub: Option<HubConfigSection>,
}

/// The `[hub]` section itself.
#[derive(Debug, Default, Deserialize)]
struct HubConfigSection {
    #[serde(default)]
    index: Option<String>,
}

/// Reads the `[hub] index` key from the config file at `path`.
///
/// A missing file is not an error (§22's config-file key is optional, one
/// of three ways to name an index URL) — only a *present but unparsable*
/// file is.
///
/// # Errors
///
/// [`CliError::Coordinator`] if `path` exists but is not valid TOML, or
/// does not match [`ConfigFile`]'s shape.
fn read_config_index(path: &Path) -> Result<Option<String>, CliError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(CliError::io(path, source)),
    };
    let config: ConfigFile = toml::from_str(&text).map_err(|source| {
        CoordinatorError::invalid(format!("`{}` is not valid TOML: {source}", path.display()))
    })?;
    Ok(config.hub.and_then(|hub| hub.index))
}

/// Resolves the index repository URL: `explicit` (an `astrs hub update
/// --index` argument), then [`hub_index::ENV_HUB_INDEX`], then the `[hub]
/// index` key of the config file at `config_path`, then
/// [`hub_index::DEFAULT_INDEX_URL`].
///
/// # Errors
///
/// As `read_config_index`, this module's own private config-file reader.
pub fn resolve_index_url(explicit: Option<&str>, config_path: &Path) -> Result<String, CliError> {
    if let Some(explicit) = explicit {
        return Ok(explicit.to_owned());
    }
    if let Ok(env) = std::env::var(hub_index::ENV_HUB_INDEX)
        && !env.trim().is_empty()
    {
        return Ok(env);
    }
    if let Some(from_config) = read_config_index(config_path)? {
        return Ok(from_config);
    }
    Ok(hub_index::DEFAULT_INDEX_URL.to_owned())
}

/// Runs the system `git` binary (blueprint §2.2: shell out, never
/// `git2`/`libgit2`) and returns its stdout.
///
/// # Errors
///
/// [`CliError::Coordinator`] if `git` cannot be spawned at all (not
/// installed, not on `PATH`), or exits non-zero — its stderr, trimmed, is
/// folded into the message so the failure is actionable without
/// re-running the command by hand.
fn run_git(working_dir: Option<&Path>, args: &[&str]) -> Result<String, CliError> {
    let mut command = Command::new("git");
    command.args(args);
    if let Some(dir) = working_dir {
        command.current_dir(dir);
    }
    let output = command.output().map_err(|source| {
        CoordinatorError::invalid(format!(
            "could not run the system `git` binary (is it installed and on `PATH`?): {source}"
        ))
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CoordinatorError::invalid(format!(
            "`git {}` failed: {}",
            args.join(" "),
            stderr.trim()
        ))
        .into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Counts `packages/*.json` entries under a local index clone, for
/// [`UpdateReport`] — best-effort: a cache directory that turns out not to
/// have a readable `packages/` at all just counts as zero rather than
/// failing an update that otherwise succeeded.
fn count_packages(cache_dir: &Path) -> usize {
    std::fs::read_dir(cache_dir.join("packages"))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry.path().extension().and_then(std::ffi::OsStr::to_str) == Some("json")
                })
                .count()
        })
        .unwrap_or(0)
}

/// `astrs hub update` arguments, independent of `clap`.
#[derive(Debug, Clone, Default)]
pub struct UpdateArgs {
    /// An explicit index URL, overriding the environment/config-file/
    /// default resolution (see [`resolve_index_url`]).
    pub index: Option<String>,
    /// The local index clone's directory. `None` resolves through
    /// [`hub_index::cache_dir_from_env`] — overridable here mainly for
    /// tests, which never touch the real cache directory.
    pub cache_dir: Option<PathBuf>,
    /// The config file [`resolve_index_url`] reads. `None` resolves
    /// through [`default_config_path`] — same rationale as `cache_dir`.
    pub config_path: Option<PathBuf>,
    /// Emit JSON rather than a human-readable summary.
    pub json: bool,
}

/// What `astrs hub update` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateReport {
    /// The index URL that was cloned or pulled.
    pub index_url: String,
    /// The local directory it was cloned/pulled into.
    pub cache_dir: PathBuf,
    /// `true` for a fresh clone, `false` for a pull against an existing
    /// clone.
    pub cloned: bool,
    /// How many `packages/*.json` entries the index has after updating.
    pub package_count: usize,
}

/// Clones the index repository if the local cache does not have it yet,
/// or fast-forward pulls it if it does.
///
/// # Errors
///
/// - As [`resolve_index_url`].
/// - [`CliError::Coordinator`] if `git clone`/`git pull --ff-only` fails —
///   including "repository not found", the expected failure against
///   [`hub_index::DEFAULT_INDEX_URL`] before it exists.
/// - [`CliError::Io`] if the cache directory's parent cannot be created.
pub fn update(out: &mut dyn Write, args: &UpdateArgs) -> Result<UpdateReport, CliError> {
    let cache_dir = args
        .cache_dir
        .clone()
        .unwrap_or_else(hub_index::cache_dir_from_env);
    let config_path = args.config_path.clone().unwrap_or_else(default_config_path);
    let index_url = resolve_index_url(args.index.as_deref(), &config_path)?;

    let cloned = !cache_dir.join(".git").is_dir();
    if cloned {
        if let Some(parent) = cache_dir.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|source| CliError::io(parent, source))?;
        }
        let cache_dir_text = cache_dir.display().to_string();
        run_git(None, &["clone", &index_url, &cache_dir_text])?;
    } else {
        run_git(Some(&cache_dir), &["pull", "--ff-only"])?;
    }

    let report = UpdateReport {
        index_url,
        package_count: count_packages(&cache_dir),
        cache_dir,
        cloned,
    };
    print_update_report(out, args.json, &report);
    Ok(report)
}

fn print_update_report(out: &mut dyn Write, json: bool, report: &UpdateReport) {
    if json {
        emit_json(
            out,
            &serde_json::json!({
                "index_url": report.index_url,
                "cache_dir": report.cache_dir.display().to_string(),
                "cloned": report.cloned,
                "package_count": report.package_count,
            }),
        );
        return;
    }
    let verb = if report.cloned { "cloned" } else { "updated" };
    let _ = writeln!(
        out,
        "{verb} {} into {} ({} package{})",
        report.index_url,
        report.cache_dir.display(),
        report.package_count,
        if report.package_count == 1 { "" } else { "s" }
    );
    let _ = out.flush();
}

/// `astrs hub search` arguments, independent of `clap`.
#[derive(Debug, Clone)]
pub struct SearchArgs {
    /// The term to match against a package's name or description
    /// (case-insensitive); an empty term matches every package.
    pub term: String,
    /// As [`UpdateArgs::cache_dir`].
    pub cache_dir: Option<PathBuf>,
    /// Emit JSON rather than a human-readable list.
    pub json: bool,
}

/// Lists every package in the local index whose name or description
/// matches `args.term`.
///
/// # Errors
///
/// [`CliError::Coordinator`] if the local index has not been fetched yet
/// (naming `astrs hub update`) or cannot be listed.
pub fn search(out: &mut dyn Write, args: &SearchArgs) -> Result<Vec<IndexPackage>, CliError> {
    let cache_dir = args
        .cache_dir
        .clone()
        .unwrap_or_else(hub_index::cache_dir_from_env);
    let hits = hub_index::search(&cache_dir, &args.term).map_err(CoordinatorError::from)?;
    print_search_results(out, args.json, &args.term, &hits);
    Ok(hits)
}

fn print_search_results(out: &mut dyn Write, json: bool, term: &str, hits: &[IndexPackage]) {
    if json {
        emit_json(
            out,
            &serde_json::json!({
                "term": term,
                "results": hits.iter().map(package_summary_json).collect::<Vec<_>>(),
            }),
        );
        return;
    }
    if hits.is_empty() {
        let _ = writeln!(out, "no packages match `{term}`");
    } else {
        for package in hits {
            let latest = package
                .versions
                .last()
                .map_or("(no versions)", |v| v.version.as_str());
            let _ = writeln!(
                out,
                "{:<24} {latest:<10} {}",
                package.name, package.description
            );
        }
    }
    let _ = out.flush();
}

fn package_summary_json(package: &IndexPackage) -> serde_json::Value {
    serde_json::json!({
        "name": package.name,
        "description": package.description,
        "latest_version": package.versions.last().map(|v| v.version.clone()),
        "version_count": package.versions.len(),
    })
}

/// `astrs hub info` arguments, independent of `clap`.
#[derive(Debug, Clone)]
pub struct InfoArgs {
    /// The package name to show.
    pub name: String,
    /// As [`UpdateArgs::cache_dir`].
    pub cache_dir: Option<PathBuf>,
    /// Emit JSON rather than a human-readable listing.
    pub json: bool,
}

/// Shows one package's description and every published version.
///
/// # Errors
///
/// [`CliError::Coordinator`] if the package (or the index itself) is not
/// found, or its entry is not valid JSON — see
/// [`hub_index::HubIndexError`].
pub fn info(out: &mut dyn Write, args: &InfoArgs) -> Result<IndexPackage, CliError> {
    let cache_dir = args
        .cache_dir
        .clone()
        .unwrap_or_else(hub_index::cache_dir_from_env);
    let package =
        hub_index::read_package(&cache_dir, &args.name).map_err(CoordinatorError::from)?;
    print_package_info(out, args.json, &package);
    Ok(package)
}

fn print_package_info(out: &mut dyn Write, json: bool, package: &IndexPackage) {
    if json {
        emit_json(
            out,
            &serde_json::json!({
                "name": package.name,
                "description": package.description,
                "versions": package.versions.iter().map(|v| serde_json::json!({
                    "version": v.version,
                    "git": v.git,
                    "rev": v.rev,
                    "subdir": v.subdir,
                })).collect::<Vec<_>>(),
            }),
        );
        return;
    }
    let _ = writeln!(out, "{}", package.name);
    let _ = writeln!(out, "  {}", package.description);
    let _ = writeln!(out, "  versions (oldest first):");
    for version in &package.versions {
        match &version.subdir {
            Some(subdir) => {
                let _ = writeln!(
                    out,
                    "    {:<10} {} @ {}  (subdir: {subdir})",
                    version.version, version.git, version.rev
                );
            }
            None => {
                let _ = writeln!(
                    out,
                    "    {:<10} {} @ {}",
                    version.version, version.git, version.rev
                );
            }
        }
    }
    let _ = out.flush();
}

/// `astrs hub init` arguments, independent of `clap`.
#[derive(Debug, Clone)]
pub struct InitArgs {
    /// The directory to scaffold the index repo layout into.
    pub dir: PathBuf,
    /// Scaffold into a non-empty directory anyway.
    pub force: bool,
    /// Emit JSON rather than a human-readable confirmation.
    pub json: bool,
}

/// Scaffolds a valid, self-hostable index repo layout at `args.dir`: a
/// `packages/` directory, an explanatory `README.md`, and a `git init` so
/// it is ready for `git add -A && git commit` — see `INDEX_README` for
/// exactly what gets written.
///
/// # Errors
///
/// - [`CliError::TargetExists`] if `args.dir` already exists and is
///   non-empty, unless `args.force` is set.
/// - [`CliError::Io`] if the layout cannot be written.
/// - [`CliError::Coordinator`] if `git init` fails.
pub fn init(out: &mut dyn Write, args: &InitArgs) -> Result<PathBuf, CliError> {
    if args.dir.exists() && !args.force {
        let non_empty = std::fs::read_dir(&args.dir)
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false);
        if non_empty {
            return Err(CliError::TargetExists {
                path: args.dir.display().to_string(),
            });
        }
    }

    let packages_dir = args.dir.join("packages");
    std::fs::create_dir_all(&packages_dir).map_err(|source| CliError::io(&packages_dir, source))?;
    // An empty directory is not something git tracks; a placeholder keeps
    // `packages/` present in the very first commit.
    std::fs::write(packages_dir.join(".gitkeep"), b"")
        .map_err(|source| CliError::io(&packages_dir, source))?;
    let readme_path = args.dir.join("README.md");
    std::fs::write(&readme_path, INDEX_README)
        .map_err(|source| CliError::io(&readme_path, source))?;

    run_git(Some(&args.dir), &["init", "--quiet"])?;

    if args.json {
        emit_json(
            out,
            &serde_json::json!({ "initialized": args.dir.display().to_string() }),
        );
    } else {
        let _ = writeln!(out, "initialized a package index at {}", args.dir.display());
        let _ = writeln!(
            out,
            "  add packages under packages/<name>.json, then `git add -A && git commit`"
        );
        let _ = out.flush();
    }
    Ok(args.dir.clone())
}

/// Writes one JSON object.
///
/// Duplicated from `command::token`'s own copy rather than shared — see
/// that module's docs on this crate's small-per-module-helper convention.
fn emit_json(out: &mut dyn Write, value: &serde_json::Value) {
    let _ = writeln!(
        out,
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_owned())
    );
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("astrs-cli-hub-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn run(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    /// Builds a real git repository at `dir` with a valid index layout
    /// (one `yolo-detector` package, two versions) already committed —
    /// the fixture every `update`/`search`/`info` end-to-end test clones
    /// or pulls from over `file://`.
    fn build_index_repo(dir: &Path) {
        std::fs::create_dir_all(dir.join("packages")).unwrap();
        std::fs::write(
            dir.join("packages").join("yolo-detector.json"),
            serde_json::to_string(&IndexPackage {
                name: "yolo-detector".to_owned(),
                description: "A YOLO-family object detector node.".to_owned(),
                versions: vec![
                    hub_index::IndexVersion {
                        version: "v0.3.0".to_owned(),
                        git: "https://example.invalid/yolo".to_owned(),
                        rev: "a1b2c3d".to_owned(),
                        subdir: None,
                    },
                    hub_index::IndexVersion {
                        version: "v0.3.1".to_owned(),
                        git: "https://example.invalid/yolo".to_owned(),
                        rev: "d4e5f6a".to_owned(),
                        subdir: None,
                    },
                ],
            })
            .unwrap(),
        )
        .unwrap();
        run(dir, &["init", "--quiet", "--initial-branch=main"]);
        run(dir, &["config", "user.email", "test@astrs.invalid"]);
        run(dir, &["config", "user.name", "astrs-cli tests"]);
        run(dir, &["add", "-A"]);
        run(dir, &["commit", "--quiet", "-m", "seed index"]);
    }

    fn file_url(dir: &Path) -> String {
        format!("file://{}", dir.display())
    }

    #[test]
    fn update_clones_a_fresh_index_and_then_pulls_it() {
        let origin = scratch("update-origin");
        build_index_repo(&origin);
        let cache = scratch("update-cache");
        std::fs::remove_dir_all(&cache).unwrap(); // update must create it

        let mut out = Vec::new();
        let report = update(
            &mut out,
            &UpdateArgs {
                index: Some(file_url(&origin)),
                cache_dir: Some(cache.clone()),
                config_path: Some(scratch("update-config-unused").join("config.toml")),
                json: false,
            },
        )
        .unwrap();
        assert!(report.cloned);
        assert_eq!(report.package_count, 1);
        assert!(cache.join("packages").join("yolo-detector.json").is_file());
        assert!(
            String::from_utf8(out).unwrap().contains("cloned"),
            "human report should say it cloned"
        );

        // A second commit lands upstream…
        std::fs::write(
            origin.join("packages").join("lidar-slam.json"),
            serde_json::to_string(&IndexPackage {
                name: "lidar-slam".to_owned(),
                description: "SLAM from a spinning lidar.".to_owned(),
                versions: vec![hub_index::IndexVersion {
                    version: "v1".to_owned(),
                    git: "https://example.invalid/slam".to_owned(),
                    rev: "abc".to_owned(),
                    subdir: None,
                }],
            })
            .unwrap(),
        )
        .unwrap();
        run(&origin, &["add", "-A"]);
        run(&origin, &["commit", "--quiet", "-m", "add lidar-slam"]);

        // …and a second `update` pulls it in, not re-cloning.
        let mut out = Vec::new();
        let report = update(
            &mut out,
            &UpdateArgs {
                index: Some(file_url(&origin)),
                cache_dir: Some(cache.clone()),
                config_path: None,
                json: false,
            },
        )
        .unwrap();
        assert!(!report.cloned, "the second update pulls, it does not clone");
        assert_eq!(report.package_count, 2);
        assert!(cache.join("packages").join("lidar-slam.json").is_file());
    }

    #[test]
    fn update_against_a_nonexistent_repository_is_a_clear_error() {
        let cache = scratch("update-missing-remote");
        std::fs::remove_dir_all(&cache).unwrap();
        let nowhere = scratch("update-missing-remote-origin");
        std::fs::remove_dir_all(&nowhere).unwrap(); // never created

        let error = update(
            &mut Vec::new(),
            &UpdateArgs {
                index: Some(file_url(&nowhere)),
                cache_dir: Some(cache),
                config_path: Some(scratch("update-missing-remote-config").join("config.toml")),
                json: false,
            },
        )
        .unwrap_err();
        assert!(matches!(error, CliError::Coordinator(_)), "{error}");
        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn search_and_info_read_the_freshly_updated_cache() {
        let origin = scratch("search-info-origin");
        build_index_repo(&origin);
        let cache = scratch("search-info-cache");
        std::fs::remove_dir_all(&cache).unwrap();
        update(
            &mut Vec::new(),
            &UpdateArgs {
                index: Some(file_url(&origin)),
                cache_dir: Some(cache.clone()),
                config_path: Some(scratch("search-info-config").join("config.toml")),
                json: false,
            },
        )
        .unwrap();

        let mut out = Vec::new();
        let hits = search(
            &mut out,
            &SearchArgs {
                term: "yolo".to_owned(),
                cache_dir: Some(cache.clone()),
                json: false,
            },
        )
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "yolo-detector");
        assert!(String::from_utf8(out).unwrap().contains("yolo-detector"));

        let mut out = Vec::new();
        let package = info(
            &mut out,
            &InfoArgs {
                name: "yolo-detector".to_owned(),
                cache_dir: Some(cache),
                json: false,
            },
        )
        .unwrap();
        assert_eq!(package.versions.len(), 2);
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("v0.3.0"));
        assert!(printed.contains("v0.3.1"));
    }

    #[test]
    fn search_before_any_update_names_the_update_command() {
        let cache = scratch("search-no-update");
        std::fs::remove_dir_all(&cache).unwrap();

        let error = search(
            &mut Vec::new(),
            &SearchArgs {
                term: String::new(),
                cache_dir: Some(cache),
                json: false,
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("astrs hub update"), "{error}");
    }

    #[test]
    fn info_on_an_unknown_package_names_search_and_update() {
        let origin = scratch("info-unknown-origin");
        build_index_repo(&origin);
        let cache = scratch("info-unknown-cache");
        std::fs::remove_dir_all(&cache).unwrap();
        update(
            &mut Vec::new(),
            &UpdateArgs {
                index: Some(file_url(&origin)),
                cache_dir: Some(cache.clone()),
                config_path: Some(scratch("info-unknown-config").join("config.toml")),
                json: false,
            },
        )
        .unwrap();

        let error = info(
            &mut Vec::new(),
            &InfoArgs {
                name: "does-not-exist".to_owned(),
                cache_dir: Some(cache),
                json: false,
            },
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("astrs hub search"), "{message}");
        assert!(message.contains("astrs hub update"), "{message}");
    }

    #[test]
    fn init_scaffolds_a_layout_update_can_then_clone_from() {
        let repo_dir = scratch("init-then-clone-repo");
        std::fs::remove_dir_all(&repo_dir).unwrap();

        init(
            &mut Vec::new(),
            &InitArgs {
                dir: repo_dir.clone(),
                force: false,
                json: false,
            },
        )
        .unwrap();
        assert!(repo_dir.join("packages").is_dir());
        assert!(repo_dir.join("README.md").is_file());
        assert!(repo_dir.join(".git").is_dir());

        // Self-hosting: add a package by hand, commit, and prove another
        // machine's `astrs hub update` can clone it back out again.
        std::fs::write(
            repo_dir.join("packages").join("yolo-detector.json"),
            serde_json::to_string(&IndexPackage {
                name: "yolo-detector".to_owned(),
                description: "detector".to_owned(),
                versions: vec![hub_index::IndexVersion {
                    version: "v1".to_owned(),
                    git: "https://example.invalid/yolo".to_owned(),
                    rev: "cafe".to_owned(),
                    subdir: None,
                }],
            })
            .unwrap(),
        )
        .unwrap();
        run(&repo_dir, &["config", "user.email", "test@astrs.invalid"]);
        run(&repo_dir, &["config", "user.name", "astrs-cli tests"]);
        run(&repo_dir, &["add", "-A"]);
        run(&repo_dir, &["commit", "--quiet", "-m", "add yolo-detector"]);

        let cache = scratch("init-then-clone-cache");
        std::fs::remove_dir_all(&cache).unwrap();
        let report = update(
            &mut Vec::new(),
            &UpdateArgs {
                index: Some(file_url(&repo_dir)),
                cache_dir: Some(cache),
                config_path: Some(scratch("init-then-clone-config").join("config.toml")),
                json: false,
            },
        )
        .unwrap();
        assert_eq!(report.package_count, 1);
    }

    #[test]
    fn init_refuses_a_nonempty_directory_without_force() {
        let dir = scratch("init-nonempty");
        std::fs::write(dir.join("keep.txt"), b"pre-existing").unwrap();

        let error = init(
            &mut Vec::new(),
            &InitArgs {
                dir: dir.clone(),
                force: false,
                json: false,
            },
        )
        .unwrap_err();
        assert!(matches!(error, CliError::TargetExists { .. }), "{error}");

        // `--force` proceeds anyway.
        init(
            &mut Vec::new(),
            &InitArgs {
                dir,
                force: true,
                json: false,
            },
        )
        .unwrap();
    }

    #[test]
    fn resolve_index_url_prefers_explicit_then_env_then_config_then_default() {
        let config_dir = scratch("resolve-index-url");
        let config_path = config_dir.join("config.toml");

        // Nothing set anywhere: the built-in default.
        assert_eq!(
            resolve_index_url(None, &config_path).unwrap(),
            hub_index::DEFAULT_INDEX_URL
        );

        // The config file's `[hub] index` key wins over the default.
        std::fs::write(
            &config_path,
            "[hub]\nindex = \"https://example.invalid/from-config\"\n",
        )
        .unwrap();
        assert_eq!(
            resolve_index_url(None, &config_path).unwrap(),
            "https://example.invalid/from-config"
        );

        // An explicit argument wins over everything.
        assert_eq!(
            resolve_index_url(Some("https://example.invalid/explicit"), &config_path).unwrap(),
            "https://example.invalid/explicit"
        );
    }

    #[test]
    fn a_malformed_config_file_is_a_clear_error() {
        let config_dir = scratch("resolve-index-url-malformed");
        let config_path = config_dir.join("config.toml");
        std::fs::write(&config_path, "not = [valid toml").unwrap();

        let error = resolve_index_url(None, &config_path).unwrap_err();
        assert!(matches!(error, CliError::Coordinator(_)), "{error}");
    }

    #[test]
    fn json_output_carries_the_same_fields_as_the_report() {
        let origin = scratch("json-output-origin");
        build_index_repo(&origin);
        let cache = scratch("json-output-cache");
        std::fs::remove_dir_all(&cache).unwrap();

        let mut out = Vec::new();
        let report = update(
            &mut out,
            &UpdateArgs {
                index: Some(file_url(&origin)),
                cache_dir: Some(cache),
                config_path: Some(scratch("json-output-config").join("config.toml")),
                json: true,
            },
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["index_url"], report.index_url);
        assert_eq!(value["package_count"], 1);
        assert_eq!(value["cloned"], true);
    }
}
