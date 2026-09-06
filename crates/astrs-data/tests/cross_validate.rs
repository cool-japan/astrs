//! Exports the bytes the out-of-workspace arrow-rs harness checks.
//!
//! `arrow` is banned inside this workspace (blueprint §18), so the other half
//! of the byte-compatibility gate — *arrow-rs reading what AstRS wrote* — runs
//! in a scratch project outside it (`arrow-golden-gen`, the same project that
//! generated `tests/golden/arrow/`). This suite is the handover: it writes
//! everything that harness needs into a directory under
//! [`std::env::temp_dir`], and checks on the way out that what it exported is
//! exactly what it meant to export.
//!
//! ```text
//! <temp>/astrs-ipc-xvalidate/
//!   golden_roundtrip/<name>.arrows   AstRS's re-encoding of every golden file
//!   native/<name>.arrows             AstRS's encoding of the synthetic corpus
//!   native/<name>.expect             the canonical rendering of that corpus
//!   README.txt                       how to run the harness over them
//! ```
//!
//! The harness then asserts, with arrow-rs and nothing of ours:
//!
//! 1. `verify-golden` — re-rendering each committed `.arrows` reproduces its
//!    `.expect` byte for byte, which pins the arrow-side renderer to the one
//!    in `tests/common/render.rs`;
//! 2. `check-roundtrip` — arrow-rs reads `golden_roundtrip/<name>.arrows` and
//!    gets a schema and batches equal to what it reads from the committed
//!    original, so AstRS's writer preserved everything arrow-rs put in;
//! 3. `check-native` — arrow-rs reads `native/<name>.arrows` and renders it to
//!    exactly the `.expect` AstRS produced, so AstRS's writer is readable by
//!    arrow-rs for shapes the golden corpus never covered (four-level nesting,
//!    all-null nested columns, zero-row nested columns, 4096-row batches).
//!
//! Nothing here fails when the harness has not been run: the export is
//! self-contained and always green, and the harness reports its own result.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "common/cases.rs"]
mod cases;
#[path = "common/render.rs"]
mod render;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use astrs_data::ipc::{IpcStreamReader, WriteOptions, to_ipc_bytes_with};
use astrs_data::{RecordBatch, Schema};

use crate::cases::cases;

/// The committed arrow-rs corpus.
fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/arrow")
}

/// The export root under the platform temporary directory.
fn export_root() -> PathBuf {
    std::env::temp_dir().join("astrs-ipc-xvalidate")
}

/// Creates `dir`, replacing whatever an earlier run left there.
fn fresh_dir(dir: &Path) -> PathBuf {
    if dir.exists() {
        std::fs::remove_dir_all(dir).unwrap_or_else(|err| panic!("{}: {err}", dir.display()));
    }
    std::fs::create_dir_all(dir).unwrap_or_else(|err| panic!("{}: {err}", dir.display()));
    dir.to_path_buf()
}

/// Decodes a stream from disk.
fn read_stream(path: &Path) -> (Arc<Schema>, Vec<RecordBatch>) {
    let bytes = std::fs::read(path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
    let mut reader = IpcStreamReader::from_slice(&bytes)
        .unwrap_or_else(|err| panic!("{}: open: {err}", path.display()));
    let batches = reader
        .read_all()
        .unwrap_or_else(|err| panic!("{}: batches: {err}", path.display()));
    (reader.schema_ref(), batches)
}

#[test]
fn export_the_golden_corpus_re_encoded_by_the_astrs_writer() {
    let out = fresh_dir(&export_root().join("golden_roundtrip"));
    let mut exported = 0usize;

    let entries = std::fs::read_dir(golden_dir()).expect("golden dir");
    for entry in entries.filter_map(std::result::Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("arrows") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("utf-8 name")
            .to_owned();

        let (schema, batches) = read_stream(&path);
        let bytes = to_ipc_bytes_with(Arc::clone(&schema), &batches, WriteOptions::new())
            .unwrap_or_else(|err| panic!("{name}: write: {err}"));
        let target = out.join(&name);
        std::fs::write(&target, bytes.as_slice())
            .unwrap_or_else(|err| panic!("{}: {err}", target.display()));

        // What we exported must still be what we decoded.
        let (again, round_tripped) = read_stream(&target);
        assert_eq!(again.as_ref(), schema.as_ref(), "{name}: schema");
        assert_eq!(
            render::render_stream(&again, &round_tripped),
            render::render_stream(&schema, &batches),
            "{name}: re-encoded stream drifted"
        );
        exported += 1;
    }

    assert!(exported >= 28, "only {exported} golden case(s) exported");
}

#[test]
fn export_the_native_corpus_with_its_canonical_rendering() {
    let out = fresh_dir(&export_root().join("native"));
    let mut exported = 0usize;

    for case in cases() {
        let bytes = to_ipc_bytes_with(Arc::clone(&case.schema), &case.batches, WriteOptions::new())
            .unwrap_or_else(|err| panic!("{}: write: {err}", case.name));
        let stream_path = out.join(format!("{}.arrows", case.name));
        std::fs::write(&stream_path, bytes.as_slice())
            .unwrap_or_else(|err| panic!("{}: {err}", stream_path.display()));

        let rendering = render::render_stream(&case.schema, &case.batches);
        let expect_path = out.join(format!("{}.expect", case.name));
        std::fs::write(&expect_path, rendering.as_bytes())
            .unwrap_or_else(|err| panic!("{}: {err}", expect_path.display()));

        // The exported bytes must render to the exported expectation, or the
        // harness would be checking arrow-rs against the wrong oracle.
        let (schema, batches) = read_stream(&stream_path);
        assert_eq!(
            render::render_stream(&schema, &batches),
            rendering,
            "{}: exported stream and expectation disagree",
            case.name
        );
        exported += 1;
    }

    assert_eq!(exported, cases().len());
    assert!(exported >= 24, "the native corpus shrank to {exported}");
}

#[test]
fn export_the_harness_instructions() {
    let root = export_root();
    std::fs::create_dir_all(&root).unwrap_or_else(|err| panic!("{}: {err}", root.display()));
    let readme = format!(
        "AstRS Arrow IPC cross-validation artefacts\n\
         =========================================\n\
         \n\
         Written by `cargo test -p astrs-data --test cross_validate`.\n\
         \n\
         golden_roundtrip/<name>.arrows  AstRS's re-encoding of tests/golden/arrow/<name>.arrows\n\
         native/<name>.arrows            AstRS's encoding of the synthetic corpus\n\
         native/<name>.expect            the canonical rendering of that corpus\n\
         \n\
         Check them with the out-of-workspace arrow-rs harness (arrow is banned\n\
         inside the workspace, so the harness lives outside it):\n\
         \n\
         cargo run --manifest-path <arrow-golden-gen>/Cargo.toml -- \\\n    \
             verify-golden   <astrs>/crates/astrs-data/tests/golden/arrow\n\
         cargo run --manifest-path <arrow-golden-gen>/Cargo.toml -- \\\n    \
             check-roundtrip <astrs>/crates/astrs-data/tests/golden/arrow {golden}\n\
         cargo run --manifest-path <arrow-golden-gen>/Cargo.toml -- \\\n    \
             check-native    {native}\n",
        golden = root.join("golden_roundtrip").display(),
        native = root.join("native").display(),
    );
    let path = root.join("README.txt");
    std::fs::write(&path, readme).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
    assert!(path.is_file());
}
