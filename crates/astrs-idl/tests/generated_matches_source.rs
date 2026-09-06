//! The drift guard: asserts `src/generated/` is byte-identical to what
//! [`support::regenerate_all`] produces from `msg-src/` right now.
//!
//! Always on (unlike `regen_write.rs`) — this is what actually keeps the
//! pre-generated `common_interfaces` set honest against the parser/
//! resolver/codegen pipeline, catching both a codegen change nobody
//! re-ran the writer for and a hand-edit of a `@generated` file (§10.3's
//! own promise: the committed tree is *mechanical* output, never hand-
//! tuned).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test-only tooling.

mod support;

use std::fs;

#[test]
fn generated_tree_matches_regeneration_from_msg_src() {
    let packages = support::regenerate_all();
    let root = support::generated_dir();
    assert!(
        root.is_dir(),
        "{} does not exist — run `cargo test -p astrs-idl --test regen_write -- --ignored` first",
        root.display()
    );

    let mut mismatches: Vec<String> = Vec::new();
    let mut expected_paths: Vec<std::path::PathBuf> = Vec::new();

    let top_level_mod_rs = support::top_level_mod_rs(&packages);
    let top_level_path = root.join("mod.rs");
    expected_paths.push(top_level_path.clone());
    check_matches(&top_level_path, &top_level_mod_rs, &mut mismatches);

    for package in &packages {
        let package_dir = root.join(&package.package_snake);
        for (file_stem, expected_content) in &package.files {
            let path = package_dir.join(format!("{file_stem}.rs"));
            expected_paths.push(path.clone());
            check_matches(&path, expected_content, &mut mismatches);
        }
        let mod_path = package_dir.join("mod.rs");
        expected_paths.push(mod_path.clone());
        check_matches(&mod_path, &package.mod_rs, &mut mismatches);
    }

    // Every generated `.rs` file under `src/generated/` must correspond to
    // something `regenerate_all` still produces — an orphan means a
    // `msg-src/` file was deleted (or renamed) without regenerating.
    let mut orphans: Vec<String> = Vec::new();
    collect_rs_files(&root, &mut orphans, &expected_paths);

    let mut report = String::new();
    if !mismatches.is_empty() {
        report.push_str(&format!(
            "{} file(s) differ from regeneration:\n",
            mismatches.len()
        ));
        for path in &mismatches {
            report.push_str("  - ");
            report.push_str(path);
            report.push('\n');
        }
    }
    if !orphans.is_empty() {
        report.push_str(&format!(
            "{} orphaned generated file(s) (no matching msg-src/ input):\n",
            orphans.len()
        ));
        for path in &orphans {
            report.push_str("  - ");
            report.push_str(path);
            report.push('\n');
        }
    }
    assert!(
        report.is_empty(),
        "\n{report}\nRun `cargo test -p astrs-idl --test regen_write -- --ignored` and commit the diff."
    );
}

fn check_matches(path: &std::path::Path, expected: &str, mismatches: &mut Vec<String>) {
    match fs::read_to_string(path) {
        Ok(actual) if actual == expected => {}
        Ok(_) => mismatches.push(path.display().to_string()),
        Err(_) => mismatches.push(format!("{} (missing)", path.display())),
    }
}

fn collect_rs_files(
    dir: &std::path::Path,
    orphans: &mut Vec<String>,
    expected: &[std::path::PathBuf],
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, orphans, expected);
        } else if path.extension().and_then(std::ffi::OsStr::to_str) == Some("rs")
            && !expected.contains(&path)
        {
            orphans.push(path.display().to_string());
        }
    }
}
