//! The regeneration pipeline shared by the `#[ignore]`d writer
//! (`regen_write.rs`, run by hand to materialize `src/generated/`) and the
//! always-on drift guard (`generated_matches_source.rs`) — both call
//! [`regenerate_all`] and [`astrs_idl::codegen::format_module`], so there is
//! exactly one text for any given `.msg`/`.srv`/`.action` file, not a
//! writer-shaped one and a checker-shaped one that could quietly drift
//! apart from each other.
//!
//! # Pipeline
//!
//! 1. [`astrs_idl::discovery::discover_source_tree`] walks `msg-src/`.
//! 2. Pass 1 parses every file and registers every `.msg` file's own type
//!    in one [`astrs_idl::resolve::TypeUniverse`] — before any codegen runs,
//!    so a package earlier in iteration order can still reference one
//!    later (cross-package resolution never depends on directory order).
//! 3. Pass 2 generates each file's `TokenStream` and formats it.
//! 4. Per-package `mod.rs` content is assembled by hand (`mod x; pub use
//!    self::x::Y;` per source file, sorted) rather than through the codegen
//!    `TokenStream` pipeline — it carries no ROS semantics of its own for
//!    `format_module` to be worth invoking over. The `self::` qualifier
//!    (`rustc`'s own suggested fix for `error[E0659]`) is load-bearing, not
//!    stylistic: `std_msgs/msg/Bool.msg` and `.../Char.msg` mean the
//!    generated tree always has a `mod bool;`/`mod char;` somewhere, and
//!    the bare (unqualified) form of `pub use bool::Bool;` is ambiguous
//!    with the builtin primitive type. See the call site in
//!    [`regenerate_all`] for the full diagnostic.

#![allow(dead_code)] // one binary per `tests/*.rs` file; not every helper is used by every one.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test-only tooling: a hard
// failure naming the offending `msg-src/` path *is* the correct behaviour here, matching every
// `#[cfg(test)] mod tests` block elsewhere in this crate.

use std::collections::BTreeMap;
use std::path::PathBuf;

use astrs_idl::ast::{ActionFile, MessageFile, ServiceFile};
use astrs_idl::codegen;
use astrs_idl::discovery::{InterfaceFile, discover_source_tree};
use astrs_idl::naming::{InterfaceKind, TypeName};
use astrs_idl::parser::{parse_action, parse_message, parse_service};
use astrs_idl::resolve::TypeUniverse;
use astrs_idl::span::{Position, Span};
use heck::ToSnakeCase;

pub fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub fn msg_src_dir() -> PathBuf {
    manifest_dir().join("msg-src")
}

pub fn generated_dir() -> PathBuf {
    manifest_dir().join("src/generated")
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
    let package = astrs_idl::naming::PackageName::new(package_snake, Span::empty(Position::START))
        .unwrap_or_else(|e| panic!("package name {package_snake:?}: {e}"));
    TypeName::new(package, kind, name, Span::empty(Position::START))
        .unwrap_or_else(|e| panic!("type name {package_snake}/{name}: {e}"))
}

/// The Rust type names one source file produces, matching
/// [`astrs_idl::codegen`]'s own naming scheme exactly (`Request`/`Response`
/// for a `.srv`; the eight-way split for an `.action`).
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
/// asserted to be valid by construction, and a panic with the offending
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
    let mut entries: Vec<Entry<'_>> = Vec::new();

    // Pass 1: parse everything, register every `.msg` file's own type.
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
    let mut mod_lines_by_package: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new(); // (file_stem, "mod x;\npub use x::{A, B};")

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
        let banner = codegen::banner_for(&relative);
        let content = codegen::format_module(&banner, tokens)
            .unwrap_or_else(|e| panic!("formatting {}: {e}", entry.file.path.display()));

        let file_stem = entry.file.type_name.to_snake_case();
        files_by_package
            .entry(entry.package_snake.clone())
            .or_default()
            .push((file_stem.clone(), content));

        let produced = produced_type_names(entry.file.kind, &entry.file.type_name);
        // `self::{file_stem}::…` rather than the bare `{file_stem}::…` a
        // human would write by hand: for the common case (a `.msg` file's
        // one type) `rustc` accepts `pub use bool::Bool;`/`pub use
        // char::Char;` too, but reports `error[E0659]` — "ambiguous ...
        // could refer to a builtin type ... could also refer to the module
        // defined here" — for exactly those two (`std_msgs/msg/Bool.msg`
        // and `.../Char.msg` are both real `common_interfaces` types, so
        // this is not a hypothetical). `rustc`'s own suggested fix is
        // `self::bool`; applying it uniformly (rather than special-casing
        // the handful of module names that happen to collide with a
        // primitive-type keyword) means one code path handles every
        // package, and `rustfmt` leaves the `self::`-prefixed form
        // untouched either way — collapsing a single-item brace list is a
        // format preference `rustfmt` applies on its own, but adding or
        // removing a `self::` qualifier is not something it ever does.
        let use_line = match produced.as_slice() {
            [single] => format!("mod {file_stem};\npub use self::{file_stem}::{single};"),
            many => {
                let pub_use = format!("pub use self::{file_stem}::{{{}}};", many.join(", "));
                // An action's eight produced names are in protocol order
                // (`Goal`, `Result`, `Feedback`, then the five wire types in
                // call sequence), not alphabetical -- see the identical note
                // in astrs-ros2's copy of this pipeline. Left unprotected,
                // `cargo fmt`'s `reorder_imports` both resorts this list
                // alphabetically and reflows it onto multiple lines
                // (verified empirically); no `common_interfaces` package is
                // an action today, so this branch is currently unreachable,
                // but it exists for the day one is added.
                if entry.file.kind == InterfaceKind::Action {
                    format!("mod {file_stem};\n#[rustfmt::skip]\n{pub_use}")
                } else {
                    format!("mod {file_stem};\n{pub_use}")
                }
            }
        };
        mod_lines_by_package
            .entry(entry.package_snake.clone())
            .or_default()
            .push((file_stem, use_line));
    }

    files_by_package
        .into_iter()
        .map(|(package_snake, mut files)| {
            files.sort_by(|a, b| a.0.cmp(&b.0));
            let mut mod_lines = mod_lines_by_package.remove(&package_snake).unwrap_or_default();
            mod_lines.sort_by(|a, b| a.0.cmp(&b.0));
            let banner = codegen::banner_for(&format!("msg-src/{package_snake}/"));
            let body = mod_lines.iter().map(|(_, line)| line.as_str()).collect::<Vec<_>>().join("\n\n");
            let mod_rs = format!(
                "{banner}\n//! `{package_snake}` — pre-generated ROS 2 common_interfaces (blueprint §10.3).\n\n{body}\n"
            );
            RegeneratedPackage { package_snake, mod_rs, files }
        })
        .collect()
}

/// The top-level `src/generated/mod.rs` content for `packages` — shared by
/// the writer and the drift guard for the same reason [`regenerate_all`]
/// itself is: one text, not a hand-duplicated one on each side that could
/// silently drift apart.
pub fn top_level_mod_rs(packages: &[RegeneratedPackage]) -> String {
    let mut lines: Vec<String> = packages
        .iter()
        .map(|package| {
            format!(
                "/// `{name}` — pre-generated ROS 2 common_interfaces.\npub mod {name};",
                name = package.package_snake
            )
        })
        .collect();
    lines.sort();

    let banner = codegen::banner_for("msg-src/");
    format!(
        "{banner}\n//! Pre-generated ROS 2 `common_interfaces` (blueprint §10.3): every type here \
         implements both `astrs_cdr::CdrSerde` and `astrs_data::AstrsMessage`, mechanically, from \
         the `.msg`/`.srv`/`.action` source under `msg-src/`. Regenerate with `cargo test -p \
         astrs-idl --test regen_write -- --ignored`; `generated_matches_source.rs` asserts this \
         tree stays byte-identical to what that regeneration produces.\n\n{}\n",
        lines.join("\n\n")
    )
}
