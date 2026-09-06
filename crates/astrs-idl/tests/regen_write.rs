//! Materializes `src/generated/` from `msg-src/` by hand: `cargo test -p
//! astrs-idl --test regen_write -- --ignored`.
//!
//! Not part of the normal test run (`#[ignore]`d) — this is the
//! regeneration entry point mentioned everywhere else in this crate as "run
//! by hand", never invoked by CI. After running it, review the diff and
//! commit it; `generated_matches_source.rs` is what keeps the committed
//! files honest afterward.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test-only tooling.

mod support;

use std::fs;

#[test]
#[ignore = "writes to src/generated/; run explicitly to regenerate common_interfaces"]
fn write_generated_common_interfaces() {
    let packages = support::regenerate_all();
    let root = support::generated_dir();

    fs::create_dir_all(&root).unwrap_or_else(|e| panic!("creating {}: {e}", root.display()));

    for package in &packages {
        let package_dir = root.join(&package.package_snake);
        fs::create_dir_all(&package_dir)
            .unwrap_or_else(|e| panic!("creating {}: {e}", package_dir.display()));

        for (file_stem, content) in &package.files {
            let path = package_dir.join(format!("{file_stem}.rs"));
            fs::write(&path, content).unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
        }
        let mod_path = package_dir.join("mod.rs");
        fs::write(&mod_path, &package.mod_rs)
            .unwrap_or_else(|e| panic!("writing {}: {e}", mod_path.display()));
    }

    let top_level = support::top_level_mod_rs(&packages);
    let top_level_path = root.join("mod.rs");
    fs::write(&top_level_path, top_level)
        .unwrap_or_else(|e| panic!("writing {}: {e}", top_level_path.display()));

    println!(
        "wrote {} package(s) under {}",
        packages.len(),
        root.display()
    );
}
