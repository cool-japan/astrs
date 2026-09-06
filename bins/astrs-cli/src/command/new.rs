//! `astrs new` (blueprint §17): scaffold a buildable node/operator crate,
//! or a starter graph manifest, wired into a starter manifest.
//!
//! Every template file lives under `src/templates/` and is embedded at
//! compile time via `include_str!` (no runtime template lookup, no path
//! that could accidentally read a file outside this crate's own source
//! tree) with placeholder tokens substituted by plain [`str::replace`] —
//! no templating engine dependency for a handful of tokens:
//! `__ASTRS_PROJECT_NAME__` (the project name, verbatim), and, where a
//! template needs it, `__ASTRS_PROJECT_TYPE__` (the operator template's
//! Rust struct name, `PascalCase`) or `__ASTRS_PROJECT_LIB_FILE_STEM__`
//! (the `operator-dylib` template's built artifact file stem, `snake_case`
//! — see `lib_file_stem`'s own docs for why a hyphenated project name
//! needs a third, distinct rendering).

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::CliError;

const NODE_CARGO_TOML: &str = include_str!("../templates/node/Cargo.toml.template");
const NODE_MAIN_RS: &str = include_str!("../templates/node/main.rs.template");
const NODE_DATAFLOW_YML: &str = include_str!("../templates/node/dataflow.yml.template");

const OPERATOR_CARGO_TOML: &str = include_str!("../templates/operator/Cargo.toml.template");
const OPERATOR_LIB_RS: &str = include_str!("../templates/operator/lib.rs.template");
const OPERATOR_DATAFLOW_YML: &str = include_str!("../templates/operator/dataflow.yml.template");

const OPERATOR_DYLIB_CARGO_TOML: &str =
    include_str!("../templates/operator-dylib/Cargo.toml.template");
const OPERATOR_DYLIB_LIB_RS: &str = include_str!("../templates/operator-dylib/lib.rs.template");
const OPERATOR_DYLIB_DATAFLOW_YML: &str =
    include_str!("../templates/operator-dylib/dataflow.yml.template");

const GRAPH_DATAFLOW_YML: &str = include_str!("../templates/graph/dataflow.yml.template");

const NAME_TOKEN: &str = "__ASTRS_PROJECT_NAME__";
const TYPE_TOKEN: &str = "__ASTRS_PROJECT_TYPE__";
const LIB_FILE_STEM_TOKEN: &str = "__ASTRS_PROJECT_LIB_FILE_STEM__";

/// Which template `astrs new` scaffolds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "lower")]
pub enum NewKind {
    /// A standalone node crate (a binary that runs as its own process).
    Node,
    /// A runtime-hosted operator crate (a library, compiled into a
    /// runtime binary via `register_operator!`).
    Operator,
    /// A dylib-hosted operator crate (a `cdylib`, `dlopen`ed at spawn time
    /// by `astrs-runtime`'s `dylib-operators` feature — blueprint §9.3,
    /// §22).
    ///
    /// An explicit `#[value(name = ...)]` rather than the enum's own
    /// `rename_all = "lower"`: that default would lowercase every letter
    /// with no word separator (`operatordylib`), while every multi-word
    /// value elsewhere in this CLI's surface (`astrs migrate from-dora`,
    /// this same command's own subcommand name below) is kebab-case.
    #[value(name = "operator-dylib")]
    OperatorDylib,
    /// A starter multi-node graph manifest, no Rust project.
    Graph,
}

/// Which language a scaffolded node/operator is written in.
///
/// Only [`NewLang::Rust`] exists in 0.1.0 (blueprint §5.3's `astrs-python`
/// is a P1 stretch crate) — a real, closed enum rather than a bare string
/// so `--lang python` fails clap's own argument parsing with a clear
/// "possible values: rust" message instead of this module inventing its
/// own "unsupported language" error for a value clap could have already
/// rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
#[value(rename_all = "lower")]
pub enum NewLang {
    /// Rust — the only language 0.1.0 scaffolds.
    #[default]
    Rust,
}

/// Arguments for `astrs new`.
#[derive(Debug, Clone)]
pub struct NewArgs {
    /// Which template to scaffold.
    pub kind: NewKind,
    /// The new project's name — also its directory name (for
    /// [`NewKind::Node`]/[`NewKind::Operator`]) or manifest file stem
    /// (for [`NewKind::Graph`]). Must be a single path component with no
    /// separators or `..` — see `validate_name` (crate-private).
    pub name: String,
    /// The language to scaffold in. Ignored for [`NewKind::Graph`]
    /// (a manifest has no language).
    pub lang: NewLang,
    /// The directory to scaffold into (the project itself becomes
    /// `dir/name/` or, for a graph, `dir/name.astrs.yml`).
    pub dir: PathBuf,
    /// Overwrite files that already exist at the target location.
    pub force: bool,
    /// Emit the report as JSON instead of the human-readable file listing.
    pub json: bool,
}

/// The result of scaffolding one new project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NewReport {
    /// Every file written, in the order they were created.
    pub files: Vec<PathBuf>,
}

/// Validate that `name` is safe to join onto a target directory: exactly
/// one plain path component, no separators, no `.`/`..`, non-empty.
///
/// # Errors
///
/// Returns [`CliError::UnsafePath`] otherwise.
fn validate_name(name: &str) -> Result<(), CliError> {
    let unsafe_path = || CliError::UnsafePath {
        path: name.to_string(),
    };
    if name.is_empty() {
        return Err(unsafe_path());
    }
    let path = Path::new(name);
    let mut components = path.components();
    match (components.next(), components.next()) {
        (Some(std::path::Component::Normal(component)), None) if component == path.as_os_str() => {
            Ok(())
        }
        _ => Err(unsafe_path()),
    }
}

/// Convert an arbitrary project name into a valid, exported Rust type
/// identifier in `PascalCase` — splitting on any run of characters that
/// are not `[A-Za-z0-9]`, capitalizing each segment's first letter, and
/// prefixing with `Op` if the result would not otherwise start with a
/// letter (a name like `"3d-tracker"` becomes `Op3DTracker`, never the
/// invalid identifier `3DTracker`).
fn pascal_case_type_name(name: &str) -> String {
    let mut out = String::new();
    for segment in name.split(|c: char| !c.is_ascii_alphanumeric()) {
        let mut chars = segment.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.extend(chars);
        }
    }
    if out.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        format!("Op{out}")
    } else if out.is_empty() {
        "Op".to_string()
    } else {
        out
    }
}

/// The file stem cargo gives a `cdylib` built from a crate named `name` —
/// every `-` replaced with `_`, matching cargo's own crate-name-to-file-name
/// rule for library artifacts (the same rule a `Cargo.toml` `name = "a-b"`
/// crate's `liba_b.so`/`.dylib`/`a_b.dll` follows). The
/// `operator-dylib` template's `dataflow.yml` uses this, not
/// [`NAME_TOKEN`] verbatim, so a hyphenated project name still produces a
/// `dylib:` path that names the file cargo actually built.
fn lib_file_stem(name: &str) -> String {
    name.replace('-', "_")
}

/// Substitute every placeholder token in `template`.
fn render(template: &str, name: &str) -> String {
    template
        .replace(NAME_TOKEN, name)
        .replace(TYPE_TOKEN, &pascal_case_type_name(name))
        .replace(LIB_FILE_STEM_TOKEN, &lib_file_stem(name))
}

/// Write `contents` to `path`, refusing to overwrite an existing file
/// unless `force`.
fn write_file(path: &Path, contents: &str, force: bool) -> Result<(), CliError> {
    if !force && path.exists() {
        return Err(CliError::TargetExists {
            path: path.display().to_string(),
        });
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| CliError::io(parent, err))?;
    }
    std::fs::write(path, contents).map_err(|err| CliError::io(path, err))
}

/// Run `astrs new`: validate `args.name`, then write every file the
/// requested [`NewKind`] template needs.
///
/// # Errors
///
/// Returns [`CliError::UnsafePath`] if `args.name` is not a single plain
/// path component, [`CliError::TargetExists`] if a target file already
/// exists and `args.force` is not set, or [`CliError::Io`] if a file
/// cannot be written.
pub fn run(out: &mut dyn Write, args: &NewArgs) -> Result<NewReport, CliError> {
    validate_name(&args.name)?;
    let _ = args.lang; // recorded for forward compatibility; only `Rust` exists today.

    let files = match args.kind {
        NewKind::Node => {
            let root = args.dir.join(&args.name);
            let entries = [
                (root.join("Cargo.toml"), NODE_CARGO_TOML),
                (root.join("src/main.rs"), NODE_MAIN_RS),
                (root.join("dataflow.yml"), NODE_DATAFLOW_YML),
            ];
            write_all(&entries, &args.name, args.force)?
        }
        NewKind::Operator => {
            let root = args.dir.join(&args.name);
            let entries = [
                (root.join("Cargo.toml"), OPERATOR_CARGO_TOML),
                (root.join("src/lib.rs"), OPERATOR_LIB_RS),
                (root.join("dataflow.yml"), OPERATOR_DATAFLOW_YML),
            ];
            write_all(&entries, &args.name, args.force)?
        }
        NewKind::OperatorDylib => {
            let root = args.dir.join(&args.name);
            let entries = [
                (root.join("Cargo.toml"), OPERATOR_DYLIB_CARGO_TOML),
                (root.join("src/lib.rs"), OPERATOR_DYLIB_LIB_RS),
                (root.join("dataflow.yml"), OPERATOR_DYLIB_DATAFLOW_YML),
            ];
            write_all(&entries, &args.name, args.force)?
        }
        NewKind::Graph => {
            let path = args.dir.join(format!("{}.astrs.yml", args.name));
            write_all(&[(path, GRAPH_DATAFLOW_YML)], &args.name, args.force)?
        }
    };

    let report = NewReport { files };

    if args.json {
        let json = serde_json::to_string_pretty(&report).unwrap_or_else(|err| {
            format!("{{\"error\": \"failed to serialize new report: {err}\"}}")
        });
        writeln!(out, "{json}").map_err(|e| CliError::io("<output>", e))?;
    } else {
        writeln!(
            out,
            "created {} file(s) for {} `{}`:",
            report.files.len(),
            kind_label(args.kind),
            args.name
        )
        .map_err(|e| CliError::io("<output>", e))?;
        for file in &report.files {
            writeln!(out, "  {}", file.display()).map_err(|e| CliError::io("<output>", e))?;
        }
    }

    Ok(report)
}

fn kind_label(kind: NewKind) -> &'static str {
    match kind {
        NewKind::Node => "node",
        NewKind::Operator => "operator",
        NewKind::OperatorDylib => "dylib operator",
        NewKind::Graph => "graph",
    }
}

fn write_all(
    entries: &[(PathBuf, &str)],
    name: &str,
    force: bool,
) -> Result<Vec<PathBuf>, CliError> {
    let mut written = Vec::with_capacity(entries.len());
    for (path, template) in entries {
        write_file(path, &render(template, name), force)?;
        written.push(path.clone());
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn scratch_dir(name: &str) -> PathBuf {
        // A monotonic counter, not just `(pid, name)`, disambiguates the
        // directory -- see `command::graph`'s own test helper for the
        // observed race this guards against if a future test ever reuses
        // an existing `name` under cargo's multi-threaded test runner.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "astrs-cli-new-test-{}-{name}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn args(kind: NewKind, name: &str, dir: PathBuf) -> NewArgs {
        NewArgs {
            kind,
            name: name.to_string(),
            lang: NewLang::Rust,
            dir,
            force: false,
            json: false,
        }
    }

    #[test]
    fn node_template_creates_cargo_toml_main_rs_and_dataflow_yml() {
        let dir = scratch_dir("node");
        let mut out = Vec::new();
        let report = run(&mut out, &args(NewKind::Node, "my-node", dir.clone())).unwrap();
        assert_eq!(report.files.len(), 3);
        for file in &report.files {
            assert!(file.exists(), "{} was not written", file.display());
        }
        let cargo_toml = std::fs::read_to_string(dir.join("my-node/Cargo.toml")).unwrap();
        assert!(cargo_toml.contains("name = \"my-node\""));
        assert!(!cargo_toml.contains(NAME_TOKEN));
        let main_rs = std::fs::read_to_string(dir.join("my-node/src/main.rs")).unwrap();
        assert!(main_rs.contains("my-node"));
        let dataflow = std::fs::read_to_string(dir.join("my-node/dataflow.yml")).unwrap();
        assert!(dataflow.contains("path: ./target/release/my-node"));
    }

    #[test]
    fn json_flag_emits_a_structured_file_list_instead_of_the_human_summary() {
        let dir = scratch_dir("json");
        let mut out = Vec::new();
        let mut a = args(NewKind::Node, "json-node", dir.clone());
        a.json = true;
        let report = run(&mut out, &a).unwrap();

        let printed = String::from_utf8(out).unwrap();
        let value: serde_json::Value = serde_json::from_str(printed.trim_end()).unwrap();
        let files = value["files"].as_array().unwrap();
        assert_eq!(files.len(), 3);
        assert_eq!(files.len(), report.files.len());
        // Every file this run actually wrote is named in the JSON, and
        // nothing else -- the "created N file(s) for node `...`:" human
        // summary line does not also appear.
        for file in &report.files {
            assert!(
                files
                    .iter()
                    .any(|v| v.as_str() == Some(&file.display().to_string()))
            );
        }
        assert!(!printed.contains("created"));
    }

    #[test]
    fn node_dataflow_yml_validates_cleanly() {
        let dir = scratch_dir("node-validates");
        let mut out = Vec::new();
        run(&mut out, &args(NewKind::Node, "camera-node", dir.clone())).unwrap();
        let yaml = std::fs::read_to_string(dir.join("camera-node/dataflow.yml")).unwrap();
        let manifest = astrs_manifest::Manifest::from_yaml_str(&yaml).unwrap();
        manifest.validate().unwrap();
    }

    #[test]
    fn operator_template_creates_lib_rs_with_a_pascal_case_type_and_validates() {
        let dir = scratch_dir("operator");
        let mut out = Vec::new();
        let report = run(
            &mut out,
            &args(NewKind::Operator, "my-cool-op", dir.clone()),
        )
        .unwrap();
        assert_eq!(report.files.len(), 3);
        let lib_rs = std::fs::read_to_string(dir.join("my-cool-op/src/lib.rs")).unwrap();
        assert!(lib_rs.contains("struct MyCoolOp"));
        assert!(lib_rs.contains("register_operator!(MyCoolOp)"));
        assert!(!lib_rs.contains(TYPE_TOKEN));

        // Plain textual assertions -- no compiler invocation -- guarding the
        // exact shape a real `cargo check` on this scaffold needs (verified
        // by hand against the live templates; this crate's tests deliberately
        // never shell out to cargo, see `generated_cargo_toml_is_textually_well_formed`
        // above). Each line below pins one way the template used to fail to
        // compile:
        //   - `OpEvent::Stop` was matched without its `{ cause, grace }`
        //     fields (`E0533`).
        //   - `Status::Stop` does not exist; `Status::Finished` is the
        //     terminal variant.
        //   - `OpEvent` is `#[non_exhaustive]`, so a downstream match (this
        //     one) needs a wildcard arm.
        //   - `register_operator!(...)` expands to a `(name, ctor)` tuple
        //     expression, which is not legal at module/item scope -- it must
        //     live inside a function body, here `operator_entry`.
        assert!(lib_rs.contains("OpEvent::Stop {"));
        assert!(lib_rs.contains("Status::Finished"));
        assert!(!lib_rs.contains("Status::Stop"));
        assert!(lib_rs.contains("_ =>"));
        assert!(lib_rs.contains("fn operator_entry()"));

        let cargo_toml = std::fs::read_to_string(dir.join("my-cool-op/Cargo.toml")).unwrap();
        // The facade's `Operator`/`OpEvent`/`Status`/`register_operator!`
        // surface is gated behind the `operator` feature (off by default,
        // see `astrs`'s own `[features]` table) -- without this, the
        // template's `use astrs::prelude::*;` would not even bring those
        // names into scope.
        assert!(cargo_toml.contains(r#"features = ["operator"]"#));

        let yaml = std::fs::read_to_string(dir.join("my-cool-op/dataflow.yml")).unwrap();
        let manifest = astrs_manifest::Manifest::from_yaml_str(&yaml).unwrap();
        manifest.validate().unwrap();
        assert_eq!(
            manifest.nodes[0].operators.as_ref().unwrap()[0].operator,
            "MyCoolOp"
        );
        // `register_operator!`'s default name is `stringify!(Type)`
        // (PascalCase) -- the same spelling the manifest's `operator:`
        // field above names (`astrs-runtime`'s `RuntimeConfig` docs: "the
        // `register_operator!` name"), asserted together so a future edit
        // to `operator_entry` cannot drift the two apart.
    }

    #[test]
    fn operator_dylib_template_creates_a_cdylib_crate_with_a_dylib_locator_and_validates() {
        let dir = scratch_dir("operator-dylib");
        let mut out = Vec::new();
        let report = run(
            &mut out,
            &args(NewKind::OperatorDylib, "my-cool-dylib-op", dir.clone()),
        )
        .unwrap();
        assert_eq!(report.files.len(), 3);

        let cargo_toml = std::fs::read_to_string(dir.join("my-cool-dylib-op/Cargo.toml")).unwrap();
        assert!(!cargo_toml.contains(NAME_TOKEN));
        assert!(cargo_toml.contains(r#"crate-type = ["cdylib", "lib"]"#));
        // `export_dylib_operator!` lives behind `astrs-operator-api`'s own
        // `dylib` feature (off by default, matching that crate's
        // `[features]` table) -- without this, `use ...
        // export_dylib_operator;` in the template would not resolve.
        assert!(cargo_toml.contains(r#"features = ["dylib"]"#));

        let lib_rs = std::fs::read_to_string(dir.join("my-cool-dylib-op/src/lib.rs")).unwrap();
        assert!(!lib_rs.contains(TYPE_TOKEN));
        assert!(lib_rs.contains("struct MyCoolDylibOp"));
        // The exported-symbol macro, not `register_operator!` -- a dylib
        // operator is loaded, never linked into a compiled-in registry.
        assert!(lib_rs.contains("export_dylib_operator!(MyCoolDylibOp)"));
        assert!(!lib_rs.contains("register_operator!"));
        // Same non-exhaustive-match and terminal-variant pitfalls as the
        // compiled-in operator template -- see that test's own comment for
        // why each of these lines is pinned rather than assumed.
        assert!(lib_rs.contains("OpEvent::Stop {"));
        assert!(lib_rs.contains("Status::Finished"));
        assert!(!lib_rs.contains("Status::Stop"));
        assert!(lib_rs.contains("_ =>"));

        let yaml = std::fs::read_to_string(dir.join("my-cool-dylib-op/dataflow.yml")).unwrap();
        let manifest = astrs_manifest::Manifest::from_yaml_str(&yaml).unwrap();
        manifest.validate().unwrap();
        let operator = &manifest.nodes[0].operators.as_ref().unwrap()[0];
        assert_eq!(operator.operator, "MyCoolDylibOp");
        assert_eq!(
            operator.dylib.as_deref(),
            // Hyphens in the project name become underscores here (cargo's
            // own `cdylib` file-naming rule — see `lib_file_stem`'s own
            // docs), not carried over verbatim the way `NAME_TOKEN` is
            // elsewhere in this same file.
            Some("./target/release/libmy_cool_dylib_op.so")
        );
        assert_eq!(operator.locator_kinds(), vec!["dylib"]);
    }

    #[test]
    fn graph_template_creates_a_single_yaml_file_and_validates() {
        let dir = scratch_dir("graph");
        let mut out = Vec::new();
        let report = run(&mut out, &args(NewKind::Graph, "perception", dir.clone())).unwrap();
        assert_eq!(report.files, vec![dir.join("perception.astrs.yml")]);
        let yaml = std::fs::read_to_string(&report.files[0]).unwrap();
        let manifest = astrs_manifest::Manifest::from_yaml_str(&yaml).unwrap();
        manifest.validate().unwrap();
        assert_eq!(manifest.name.as_deref(), Some("perception"));
    }

    #[test]
    fn path_traversal_in_name_is_rejected() {
        let dir = scratch_dir("traversal");
        for bad in ["../escape", "/etc/passwd", "a/b", "..", "."] {
            let err = run(&mut Vec::new(), &args(NewKind::Node, bad, dir.clone())).unwrap_err();
            assert!(
                matches!(err, CliError::UnsafePath { .. }),
                "name `{bad}` should be rejected, got {err:?}"
            );
        }
    }

    #[test]
    fn empty_name_is_rejected() {
        let dir = scratch_dir("empty");
        let err = run(&mut Vec::new(), &args(NewKind::Node, "", dir)).unwrap_err();
        assert!(matches!(err, CliError::UnsafePath { .. }));
    }

    #[test]
    fn existing_target_without_force_is_rejected() {
        let dir = scratch_dir("exists");
        run(&mut Vec::new(), &args(NewKind::Node, "dup", dir.clone())).unwrap();
        let err = run(&mut Vec::new(), &args(NewKind::Node, "dup", dir)).unwrap_err();
        assert!(matches!(err, CliError::TargetExists { .. }));
    }

    #[test]
    fn force_overwrites_an_existing_target() {
        let dir = scratch_dir("force");
        run(&mut Vec::new(), &args(NewKind::Node, "dup", dir.clone())).unwrap();
        let mut a = args(NewKind::Node, "dup", dir);
        a.force = true;
        run(&mut Vec::new(), &a).unwrap();
    }

    #[test]
    fn pascal_case_handles_hyphens_underscores_and_leading_digits() {
        assert_eq!(pascal_case_type_name("my-cool-op"), "MyCoolOp");
        assert_eq!(pascal_case_type_name("my_cool_op"), "MyCoolOp");
        // "3d" is one segment (digits and letters are both alphanumeric,
        // so no split occurs between them) -- only its first character
        // (itself a digit, case-less) is touched by the capitalization
        // step, and the whole identifier is `Op`-prefixed because it
        // would otherwise start with a digit.
        assert_eq!(pascal_case_type_name("3d-tracker"), "Op3dTracker");
        assert_eq!(pascal_case_type_name("simple"), "Simple");
    }

    #[test]
    fn lib_file_stem_turns_hyphens_into_underscores_and_leaves_everything_else_alone() {
        assert_eq!(lib_file_stem("my-cool-op"), "my_cool_op");
        assert_eq!(lib_file_stem("my_cool_op"), "my_cool_op");
        assert_eq!(lib_file_stem("simple"), "simple");
        assert_eq!(lib_file_stem("3d-tracker"), "3d_tracker");
    }

    #[test]
    fn generated_cargo_toml_is_textually_well_formed() {
        let dir = scratch_dir("cargo-toml-shape");
        run(
            &mut Vec::new(),
            &args(NewKind::Node, "shape-check", dir.clone()),
        )
        .unwrap();
        let cargo_toml = std::fs::read_to_string(dir.join("shape-check/Cargo.toml")).unwrap();
        // Plain textual assertions -- no `toml` crate dependency, and
        // deliberately not a real `cargo metadata` invocation (slow,
        // network-touching in the worst case); see this crate's own test
        // suite conventions.
        assert!(cargo_toml.contains("[package]"));
        assert!(cargo_toml.contains("[dependencies]"));
        assert!(
            cargo_toml
                .lines()
                .any(|l| l.trim() == "name = \"shape-check\"")
        );
        assert!(!cargo_toml.contains("workspace = true"));
    }
}
