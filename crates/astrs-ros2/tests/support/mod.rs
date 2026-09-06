//! The `msg-src/` → `src/interfaces/` regeneration pipeline.
//!
//! Shared by the `#[ignore]`d writer (`regen_write.rs`, run by hand to
//! materialize `src/interfaces/`) and the always-on drift guard
//! (`interfaces_match_source.rs`), so there is exactly one text for any
//! given `.msg`/`.srv`/`.action` file rather than a writer-shaped one and a
//! checker-shaped one that could quietly drift apart.
//!
//! This mirrors `astrs-idl`'s own `tests/support/mod.rs` — deliberately, so
//! that the two generated trees in this repository are produced by the same
//! shape of pipeline — with exactly one addition, described next.
//!
//! # The `astrs_idl` path shim
//!
//! `astrs-idl`'s code generator emits **absolute** `astrs_idl::…` paths:
//! `astrs_idl::runtime::ColumnValue` for the columnar contract, and
//! `astrs_idl::generated::<pkg>::<Type>` for a cross-package field
//! reference. That is correct for `astrs-idl`'s own tree, where
//! `extern crate self as astrs_idl` makes both resolve, and it is correct
//! for a downstream crate that generates *leaf* packages. It is not correct
//! here: `action_msgs/msg/GoalInfo.msg` references
//! `unique_identifier_msgs/UUID`, which lives in **this** crate's generated
//! tree, not in `astrs_idl::generated`.
//!
//! Rather than rewrite the generated text — which would make this crate's
//! copy of every type textually different from what `astrs-idl` produces,
//! and the drift guard correspondingly weaker — each generated file opens
//! with one import:
//!
//! ```text
//! use crate::idl as astrs_idl;
//! ```
//!
//! [`crate::idl`](../../src/idl.rs) re-exports `astrs_idl::runtime`
//! verbatim and merges the two `generated` trees, so every emitted path
//! resolves without a single token of the generated body being touched. The
//! import is added through the *banner*, which `format_module` prepends as
//! raw text ahead of the formatted items.

#![allow(dead_code)] // one binary per `tests/*.rs` file; not every helper is used by every one.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test-only tooling: a hard
// failure naming the offending `msg-src/` path *is* the correct behaviour here.

use std::collections::BTreeMap;
use std::path::PathBuf;

use astrs_idl::ast::{ActionFile, MessageFile, ServiceFile};
use astrs_idl::codegen;
use astrs_idl::discovery::{InterfaceFile, discover_source_tree};
use astrs_idl::naming::{InterfaceKind, PackageName, TypeName};
use astrs_idl::parser::{parse_action, parse_message, parse_service};
use astrs_idl::resolve::TypeUniverse;
use astrs_idl::span::{Position, Span};
use heck::ToSnakeCase;

/// The one import every generated file opens with. See the module docs.
pub const PATH_SHIM: &str = "use crate::idl as astrs_idl;";

/// Types this crate's `msg-src/` references but does not own.
///
/// `builtin_interfaces/Time` and `/Duration` ship pre-generated inside
/// `astrs-idl`; they are registered in the universe by name so field
/// references resolve, and `rust_path_for` then emits
/// `astrs_idl::generated::builtin_interfaces::Time`, which [`crate::idl`]
/// forwards to the real one.
pub const EXTERNAL_TYPES: &[(&str, &str)] = &[
    ("builtin_interfaces", "Time"),
    ("builtin_interfaces", "Duration"),
];

pub fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub fn msg_src_dir() -> PathBuf {
    manifest_dir().join("msg-src")
}

pub fn generated_dir() -> PathBuf {
    manifest_dir().join("src/interfaces")
}

/// One package's regenerated file tree.
pub struct RegeneratedPackage {
    pub package_snake: String,
    pub mod_rs: String,
    /// `(file_stem, content)`, sorted by `file_stem`.
    pub files: Vec<(String, String)>,
}

enum Parsed {
    Message(MessageFile),
    Service(ServiceFile),
    Action(ActionFile),
}

fn type_name_of(package_snake: &str, kind: InterfaceKind, name: &str) -> TypeName {
    let package = PackageName::new(package_snake, Span::empty(Position::START))
        .unwrap_or_else(|e| panic!("package name {package_snake:?}: {e}"));
    TypeName::new(package, kind, name, Span::empty(Position::START))
        .unwrap_or_else(|e| panic!("type name {package_snake}/{name}: {e}"))
}

/// The Rust type names one source file produces, matching
/// [`astrs_idl::codegen`]'s own naming scheme exactly.
fn produced_type_names(kind: InterfaceKind, base_name: &str) -> Vec<String> {
    match kind {
        InterfaceKind::Msg => vec![base_name.to_owned()],
        InterfaceKind::Srv => vec![
            format!("{base_name}Request"),
            format!("{base_name}Response"),
        ],
        InterfaceKind::Action => vec![
            format!("{base_name}Goal"),
            format!("{base_name}Result"),
            format!("{base_name}Feedback"),
            format!("{base_name}SendGoalRequest"),
            format!("{base_name}SendGoalResponse"),
            format!("{base_name}GetResultRequest"),
            format!("{base_name}GetResultResponse"),
            format!("{base_name}FeedbackMessage"),
        ],
    }
}

/// Runs the whole `msg-src/` → generated-source pipeline in memory.
///
/// Panics (rather than returning a `Result`) on any parse/resolve/codegen
/// failure: both callers are test binaries where every `msg-src/` file is
/// asserted to be valid by construction, and a panic naming the offending
/// path is a better test failure than threading `Result` through two
/// `#[test]` entry points that would just `.expect()` it anyway.
#[allow(clippy::too_many_lines)]
pub fn regenerate_all() -> Vec<RegeneratedPackage> {
    let packages = discover_source_tree(&msg_src_dir())
        .unwrap_or_else(|e| panic!("discovering msg-src at {}: {e}", msg_src_dir().display()));
    assert!(
        !packages.is_empty(),
        "msg-src/ produced no packages — check the fixture layout"
    );

    struct Entry<'a> {
        package_snake: String,
        file: &'a InterfaceFile,
        parsed: Parsed,
    }

    let mut universe = TypeUniverse::new();
    for (package, name) in EXTERNAL_TYPES {
        universe.register(type_name_of(package, InterfaceKind::Msg, name));
    }
    let mut entries: Vec<Entry<'_>> = Vec::new();

    // Pass 1: parse everything, register every `.msg` file's own type — before
    // any codegen runs, so a package earlier in iteration order can still
    // reference one later.
    for package in &packages {
        for file in &package.interfaces {
            let source = std::fs::read_to_string(&file.path)
                .unwrap_or_else(|e| panic!("reading {}: {e}", file.path.display()));
            let parsed = match file.kind {
                InterfaceKind::Msg => Parsed::Message(
                    parse_message(&source)
                        .unwrap_or_else(|e| panic!("parsing {}: {e}", file.path.display())),
                ),
                InterfaceKind::Srv => Parsed::Service(
                    parse_service(&source)
                        .unwrap_or_else(|e| panic!("parsing {}: {e}", file.path.display())),
                ),
                InterfaceKind::Action => Parsed::Action(
                    parse_action(&source)
                        .unwrap_or_else(|e| panic!("parsing {}: {e}", file.path.display())),
                ),
            };
            if file.kind == InterfaceKind::Msg {
                universe.register(type_name_of(
                    package.name.as_str(),
                    InterfaceKind::Msg,
                    &file.type_name,
                ));
            }
            entries.push(Entry {
                package_snake: package.name.as_str().to_owned(),
                file,
                parsed,
            });
        }
    }

    // Pass 2: generate.
    let mut files_by_package: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut mod_lines_by_package: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();

    for entry in &entries {
        let type_name = type_name_of(&entry.package_snake, entry.file.kind, &entry.file.type_name);
        let tokens = match &entry.parsed {
            Parsed::Message(file) => {
                codegen::generate_message(&type_name, &file.section, &universe)
            }
            Parsed::Service(file) => codegen::generate_service(&type_name, file, &universe),
            Parsed::Action(file) => codegen::generate_action(&type_name, file, &universe),
        }
        .unwrap_or_else(|e| panic!("generating {}: {e}", entry.file.path.display()));

        let relative = entry
            .file
            .path
            .strip_prefix(msg_src_dir())
            .unwrap_or(&entry.file.path)
            .to_string_lossy()
            .replace('\\', "/");
        let banner = format!("{}\n{PATH_SHIM}", codegen::banner_for(&relative));
        let content = codegen::format_module(&banner, tokens)
            .unwrap_or_else(|e| panic!("formatting {}: {e}", entry.file.path.display()));

        let file_stem = entry.file.type_name.to_snake_case();
        files_by_package
            .entry(entry.package_snake.clone())
            .or_default()
            .push((file_stem.clone(), content));

        let produced = produced_type_names(entry.file.kind, &entry.file.type_name);
        // `self::{file_stem}::…` rather than the bare form, matching
        // `astrs-idl`'s own generated tree: for a source file whose snake_case
        // stem collides with a builtin type name (`Bool.msg` → `mod bool;`)
        // the unqualified `pub use bool::Bool;` is `error[E0659]`.
        //
        // An action's eight produced names come out of `produced_type_names`
        // in protocol order (`Goal`, `Result`, `Feedback`, then the five
        // wire types in call sequence) rather than alphabetical — that is
        // the order a ROS 2 reader expects, not an oversight. Left alone,
        // `cargo fmt`'s stable `reorder_imports` pass resorts any run of
        // `pub use` lines it does not skip back to alphabetical, which is
        // exactly what silently happened to the committed
        // `example_interfaces/mod.rs` the one time this crate's `cargo fmt
        // --all` ran over it. `#[rustfmt::skip]` on only the first line of a
        // run does not protect the run — rustfmt still resorts every
        // unskipped line around it — so every line needs its own (verified
        // empirically). `Msg` (one produced name) and `Srv`
        // (`Request`/`Response`, always alphabetical: both share the base
        // name as a prefix, and `"Request" < "Response"` for any prefix) can
        // never actually be resorted either way, so they are left as plain
        // lines rather than carrying a no-op attribute.
        let uses = produced
            .iter()
            .map(|name| {
                if entry.file.kind == InterfaceKind::Action {
                    format!("#[rustfmt::skip]\npub use self::{file_stem}::{name};")
                } else {
                    format!("pub use self::{file_stem}::{name};")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        mod_lines_by_package
            .entry(entry.package_snake.clone())
            .or_default()
            .push((file_stem.clone(), format!("mod {file_stem};\n{uses}")));
    }

    let mut regenerated = Vec::new();
    for (package_snake, mut files) in files_by_package {
        files.sort_by(|left, right| left.0.cmp(&right.0));
        let mut mod_lines = mod_lines_by_package
            .remove(&package_snake)
            .unwrap_or_default();
        mod_lines.sort_by(|left, right| left.0.cmp(&right.0));

        let body = mod_lines
            .into_iter()
            .map(|(_, text)| text)
            .collect::<Vec<_>>()
            .join("\n\n");
        let mod_rs = format!(
            "{}\n//! `{package_snake}` — ROS 2 interfaces this crate generates for itself \
             (blueprint §10.4).\n\n{body}\n",
            codegen::banner_for("msg-src/"),
        );

        regenerated.push(RegeneratedPackage {
            package_snake,
            mod_rs,
            files,
        });
    }
    regenerated
}

/// The `src/interfaces/mod.rs` that ties the regenerated packages together.
pub fn top_level_mod_rs(packages: &[RegeneratedPackage]) -> String {
    let mods = packages
        .iter()
        .map(|package| {
            let name = &package.package_snake;
            format!("/// `{name}` — ROS 2 interfaces generated from `msg-src/`.\npub mod {name};")
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    format!(
        "{}\n//! The ROS 2 interface packages an rcl-level client library needs but \
         `astrs-idl`'s `common_interfaces` set does not carry (blueprint §10.4): the \
         parameter, action and example packages, generated from `msg-src/` by the same \
         `astrs-idl` pipeline. Regenerate with `cargo test -p astrs-ros2 --test regen_write \
         -- --ignored`; `interfaces_match_source.rs` asserts this tree stays byte-identical \
         to what that regeneration produces.\n//!\n//! `rmw_dds_common`'s three graph types \
         are **not** here: their `Gid` member changes width with \
         [`RosCompat`](astrs_rtps::discovery::RosCompat), which a fixed-width generated \
         struct cannot express, so they are hand-written in [`crate::graph::wire`].\n\n{mods}\n",
        codegen::banner_for("msg-src/"),
    )
}
