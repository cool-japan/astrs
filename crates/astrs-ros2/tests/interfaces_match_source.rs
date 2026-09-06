//! The drift guard: `src/interfaces/` must be byte-identical to what
//! `msg-src/` regenerates.
//!
//! Generated code that is committed is code somebody will eventually edit by
//! hand — a one-line "quick fix" that the next regeneration silently
//! discards. This test makes that impossible: it runs the exact pipeline
//! `regen_write.rs` runs and compares the result to what is on disk,
//! in memory, on every `cargo test`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test-only tooling.

mod support;

use std::collections::BTreeSet;
use std::fs;

/// Read one committed file, failing with the path rather than a bare
/// `NotFound`.
fn read(path: &std::path::Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| {
        panic!(
            "{} is missing or unreadable ({error}); run \
             `cargo test -p astrs-ros2 --test regen_write -- --ignored`",
            path.display()
        )
    })
}

#[test]
fn every_generated_file_matches_its_source() {
    let packages = support::regenerate_all();
    let root = support::generated_dir();

    for package in &packages {
        let package_dir = root.join(&package.package_snake);
        for (file_stem, expected) in &package.files {
            let path = package_dir.join(format!("{file_stem}.rs"));
            let actual = read(&path);
            assert_eq!(
                &actual,
                expected,
                "{} has drifted from `msg-src/`; regenerate with \
                 `cargo test -p astrs-ros2 --test regen_write -- --ignored`",
                path.display()
            );
        }
        let mod_path = package_dir.join("mod.rs");
        assert_eq!(
            &read(&mod_path),
            &package.mod_rs,
            "{} has drifted from `msg-src/`",
            mod_path.display()
        );
    }

    let top_level_path = root.join("mod.rs");
    assert_eq!(
        read(&top_level_path),
        support::top_level_mod_rs(&packages),
        "{} has drifted from `msg-src/`",
        top_level_path.display()
    );
}

#[test]
fn no_stale_files_linger_in_the_generated_tree() {
    let packages = support::regenerate_all();
    let root = support::generated_dir();

    let expected_dirs: BTreeSet<String> = packages
        .iter()
        .map(|package| package.package_snake.clone())
        .collect();
    let mut actual_dirs = BTreeSet::new();
    for entry in fs::read_dir(&root).unwrap_or_else(|e| panic!("reading {}: {e}", root.display())) {
        let entry = entry.expect("a directory entry");
        if entry.file_type().expect("a file type").is_dir() {
            actual_dirs.insert(entry.file_name().to_string_lossy().into_owned());
        }
    }
    assert_eq!(
        actual_dirs,
        expected_dirs,
        "a package directory under {} has no counterpart in msg-src/ (or vice versa)",
        root.display()
    );

    for package in &packages {
        let package_dir = root.join(&package.package_snake);
        let expected_files: BTreeSet<String> = package
            .files
            .iter()
            .map(|(stem, _)| format!("{stem}.rs"))
            .chain(std::iter::once("mod.rs".to_owned()))
            .collect();
        let mut actual_files = BTreeSet::new();
        for entry in fs::read_dir(&package_dir)
            .unwrap_or_else(|e| panic!("reading {}: {e}", package_dir.display()))
        {
            let entry = entry.expect("a directory entry");
            actual_files.insert(entry.file_name().to_string_lossy().into_owned());
        }
        assert_eq!(
            actual_files,
            expected_files,
            "a file under {} has no counterpart in msg-src/ (or vice versa)",
            package_dir.display()
        );
    }
}

#[test]
fn every_generated_file_opens_with_the_path_shim() {
    let packages = support::regenerate_all();
    for package in &packages {
        for (file_stem, content) in &package.files {
            assert!(
                content.contains(support::PATH_SHIM),
                "{}/{file_stem}.rs does not import the `astrs_idl` path shim",
                package.package_snake
            );
        }
    }
}
