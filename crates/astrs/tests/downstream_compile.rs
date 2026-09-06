//! The guard `tests/one_dependency.rs` structurally cannot be.
//!
//! `one_dependency.rs` proves that every name an application needs is
//! *re-exported* by this facade. It cannot prove that a real downstream crate
//! can *compile* against it, because of how cargo builds integration tests: a
//! package's own `tests/` see that package's regular dependencies, so
//! `astrs_data`, `astrs_wire` and every other underlying crate sit in the
//! extern prelude whether the test file names them or not. A macro expansion
//! that emits `::astrs_data::…` therefore resolves there — and fails for the
//! §9.1 application whose entire `[dependencies]` section is `astrs = "0.1"`,
//! where `astrs-data` is a *transitive* dependency and so is not nameable at
//! all (`error[E0433]: cannot find astrs_data in the crate root`).
//!
//! The only way to test the real thing is to *be* the real thing. Each test
//! below writes a throwaway crate into [`std::env::temp_dir`] whose sole
//! dependency is a path dependency on this facade, drops blueprint §9.1's and
//! §9.3's samples into it, and runs `cargo check` on it as a subprocess.
//!
//! # What is checked, and what a failure means
//!
//! | Test | Claim |
//! |---|---|
//! | [`the_flagship_sample_compiles_with_astrs_as_the_only_dependency`] | §9.1's node, verbatim in spirit: the derive, `Node::init_from_env`, a typed output, `data.view()` into an `ImageView`, `meta.follow()` |
//! | [`the_operator_sample_compiles_with_astrs_as_the_only_dependency`] | §9.3's operator: the trait, `register_operator!` at the facade root *and* through the prelude, `OperatorRegistry::from_entries`, and `#[astrs::operator]` |
//! | [`a_broken_sample_really_does_fail_to_compile`] | the harness is not vacuous — a deliberately malformed URN must be *rejected* |
//!
//! A failure here is never a documentation problem. It means an item is
//! unreachable, or a macro emitted a path that only resolves inside this
//! workspace.
//!
//! # Cost, and how it is kept down
//!
//! Every scaffold shares one `CARGO_TARGET_DIR` under the temp directory, at
//! a stable path: the first run in a fresh environment compiles the facade's
//! dependency tree once (`cargo check`, not a full build), and every run after
//! that — including the other tests in this file, which cargo serialises on
//! that directory's own lock — reuses it.
//!
//! Resolution is attempted `--offline` first, because a release gate should
//! not depend on a network round-trip; the retry without it exists for a
//! genuinely cold `~/.cargo` and is what the failure is reported from.

#![cfg(feature = "node")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// This package's directory, from which the facade's own path is derived.
///
/// A compile-time constant cargo supplies, never a literal: the repository
/// may live anywhere.
const FACADE_DIR: &str = env!("CARGO_MANIFEST_DIR");

/// The shared `CARGO_TARGET_DIR` every scaffold builds into (see module docs).
fn shared_target_dir() -> PathBuf {
    std::env::temp_dir().join("astrs-downstream-compile-target")
}

/// A fresh, empty directory for one scaffold.
///
/// Keyed by process id *and* a per-call counter, matching the convention
/// `astrs-cli`'s own scaffold tests use: nextest runs each test in its own
/// process, but a future test that scaffolds twice must not collide with
/// itself.
fn scratch_dir(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "astrs-downstream-{}-{name}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).expect("the scratch directory is writable");
    dir
}

/// Writes a throwaway crate: a manifest naming only `astrs` (by path, with
/// `features`), and one `src/main.rs`.
///
/// The `[workspace]` table is what keeps cargo from trying to adopt the
/// scaffold into a workspace it happens to sit inside — required, even though
/// the temp directory is normally nowhere near one.
///
/// The facade's path goes in as a TOML *literal* string (single quotes), which
/// processes no escapes: a Windows path's backslashes survive it unmangled.
fn scaffold(dir: &Path, name: &str, features: &str, main_rs: &str) {
    let manifest = format!(
        "[package]\n\
         name = \"{name}\"\n\
         version = \"0.0.0\"\n\
         edition = \"2024\"\n\
         publish = false\n\
         \n\
         # Not a member of any workspace, and deliberately so: this crate has\n\
         # to resolve exactly as a stranger's crate would.\n\
         [workspace]\n\
         \n\
         [dependencies]\n\
         astrs = {{ path = '{facade}'{features} }}\n",
        facade = FACADE_DIR,
        features = features,
    );
    std::fs::write(dir.join("Cargo.toml"), manifest).expect("the manifest is writable");
    std::fs::write(dir.join("src/main.rs"), main_rs).expect("the source file is writable");
}

/// The cargo to drive the child build with.
///
/// `CARGO` is set by cargo itself for anything it runs, so this picks the
/// *same* toolchain that built this test rather than whatever `PATH` resolves
/// to — which matters under `rustup`, where a bare `cargo` may be a different
/// channel.
fn cargo_binary() -> PathBuf {
    std::env::var_os("CARGO").map_or_else(|| PathBuf::from("cargo"), PathBuf::from)
}

/// Runs `cargo check` on the scaffold at `dir`, returning the child's output.
///
/// The inherited environment is scrubbed of the variables cargo sets for
/// *this* build: left in place, they would point the child at this
/// workspace's manifest and target directory, or wrap its `rustc` in a
/// workspace-specific driver, and the child would no longer be a faithful
/// stand-in for a stranger's build.
fn cargo_check(dir: &Path, offline: bool) -> std::io::Result<Output> {
    let mut command = Command::new(cargo_binary());
    command.arg("check").arg("--quiet");
    if offline {
        command.arg("--offline");
    }
    command
        .current_dir(dir)
        .env("CARGO_TARGET_DIR", shared_target_dir())
        .env_remove("CARGO_MANIFEST_DIR")
        .env_remove("CARGO_MANIFEST_PATH")
        .env_remove("CARGO_PRIMARY_PACKAGE")
        .env_remove("CARGO_BUILD_TARGET_DIR")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("CARGO_BUILD_RUSTFLAGS")
        .env_remove("CARGO_MAKEFLAGS")
        .output()
}

/// The outcome of checking one scaffold: whether it compiled, and everything
/// needed to explain why not.
struct CheckOutcome {
    /// Whether `cargo check` reported success.
    compiled: bool,
    /// The child's combined diagnostics, ready to print.
    diagnostics: String,
}

/// Scaffolds a crate and checks it, offline first (see module docs).
///
/// Returns [`None`] when `cargo` itself could not be started at all — an
/// environment without a toolchain on `PATH`, which is not a fact about this
/// facade and must not be reported as one.
fn check_scaffold(name: &str, features: &str, main_rs: &str) -> Option<CheckOutcome> {
    let dir = scratch_dir(name);
    scaffold(&dir, name, features, main_rs);

    let output = match cargo_check(&dir, true) {
        Ok(output) if output.status.success() => output,
        // Anything else is retried online: a first run against a cold
        // `~/.cargo` cannot resolve the registry offline, and the retry's
        // result is the one reported.
        Ok(_) => match cargo_check(&dir, false) {
            Ok(output) => output,
            Err(error) => return spawn_failure(&error),
        },
        Err(error) => return spawn_failure(&error),
    };

    let outcome = CheckOutcome {
        compiled: output.status.success(),
        diagnostics: format!(
            "--- stdout ---\n{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ),
    };
    // Only a *successful* scaffold is cleaned up: a failing one is left on
    // disk so its `cargo check` can be re-run by hand.
    if outcome.compiled {
        let _ = std::fs::remove_dir_all(&dir);
    } else {
        eprintln!("scaffold left in place for inspection: {}", dir.display());
    }
    Some(outcome)
}

/// Reports a `cargo` that could not be spawned, and yields [`None`].
fn spawn_failure(error: &std::io::Error) -> Option<CheckOutcome> {
    eprintln!(
        "skipping the downstream compile check: cargo could not be started ({error}). \
         This test proves nothing in this environment."
    );
    None
}

/// Blueprint §9.1's flagship node, as an application actually writes it.
///
/// Kept as one literal rather than assembled from pieces: the point is that
/// *this text*, the text the blueprint shows a reader, compiles.
const FLAGSHIP_MAIN: &str = r#"
use astrs::prelude::*;

#[derive(AstrsMessage)]
#[astrs(urn = "std/vision/v1/Detections")]
struct Detections {
    boxes: Vec<[f32; 4]>,
    scores: Vec<f32>,
    labels: Vec<u32>,
}

/// Stands in for the real model: the sample's point is the surface around
/// it, not the inference.
fn run_model(img: &ImageView) -> Result<Detections, NodeError> {
    let _ = (img.width(), img.height());
    Ok(Detections {
        boxes: Vec::new(),
        scores: Vec::new(),
        labels: Vec::new(),
    })
}

/// The general n-dimensional view, reachable from the same prelude.
fn tensor_view_is_reachable(view: Option<TensorView<f32>>) -> bool {
    view.is_none()
}

/// The derive's escape hatch, aimed at the facade's own re-export chain —
/// the path the automatic resolution produces for this very crate.
#[derive(AstrsMessage)]
#[astrs(urn = "std/test/v1/Reading", crate = "::astrs::__private::data")]
struct Reading {
    values: Vec<f64>,
}

fn main() -> Result<(), NodeError> {
    assert_eq!(<Reading as AstrsMessage>::URN, "std/test/v1/Reading");
    assert!(tensor_view_is_reachable(None));

    let (mut node, mut events) = Node::init_from_env()?;
    let mut detections = node.output::<Detections>("detections")?;

    while let Some(event) = events.recv() {
        match event {
            Event::Input { id, data, meta } if id == "frames" => {
                let img: ImageView = data.view()?;
                detections.send(run_model(&img)?, meta.follow())?;
            }
            Event::InputClosed { .. } => {}
            Event::Stop(_) => break,
            _ => {}
        }
    }
    Ok(())
}
"#;

/// Blueprint §9.3's operator, plus the three ways its registry entry can be
/// spelled through the facade.
const OPERATOR_MAIN: &str = r#"
use astrs::prelude::*;

#[derive(Default)]
struct Crop {
    seen: usize,
}

impl Operator for Crop {
    fn on_event(&mut self, event: &OpEvent, out: &mut OpOutput) -> OpResult<Status> {
        match event {
            OpEvent::Input { id, metadata, payload, .. } => {
                self.seen += 1;
                out.send_bytes(id.as_str(), metadata.follow(), payload.clone())?;
                Ok(Status::Continue)
            }
            OpEvent::Stop { .. } => Ok(Status::Finished),
            _ => Ok(Status::Continue),
        }
    }
}

/// The attribute form, which generates the same entry the macro does.
#[astrs::operator("crop-v2")]
#[derive(Default)]
struct Sharpen;

impl Operator for Sharpen {
    fn on_event(&mut self, _event: &OpEvent, _out: &mut OpOutput) -> OpResult<Status> {
        Ok(Status::Continue)
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // §9.3, verbatim: the macro at the facade root.
    let registry = OperatorRegistry::from_entries([astrs::register_operator!(Crop)])?;
    assert!(registry.contains("Crop"));

    // The same macro, imported by the prelude glob rather than path-qualified.
    let from_prelude = OperatorRegistry::from_entries([register_operator!(Crop)])?;
    assert!(from_prelude.contains("Crop"));

    // And the attribute's generated entry, which must be interchangeable.
    let from_attribute = OperatorRegistry::from_entries([Sharpen::operator_entry()])?;
    assert!(from_attribute.contains("crop-v2"));

    Ok(())
}
"#;

/// A crate that must *not* compile: the URN is syntactically invalid, so the
/// derive's own `compile_error!` has to stop it.
const BROKEN_MAIN: &str = r#"
use astrs::prelude::*;

#[derive(AstrsMessage)]
#[astrs(urn = "this is not a urn")]
struct Broken {
    values: Vec<f64>,
}

fn main() {
    let _ = Broken { values: Vec::new() };
}
"#;

#[test]
fn the_flagship_sample_compiles_with_astrs_as_the_only_dependency() {
    let Some(outcome) = check_scaffold("flagship", "", FLAGSHIP_MAIN) else {
        return;
    };
    assert!(
        outcome.compiled,
        "blueprint §9.1's flagship sample does not compile for a crate whose only \
         dependency is `astrs` — the facade is not, in fact, enough:\n{}",
        outcome.diagnostics
    );
}

#[test]
fn the_operator_sample_compiles_with_astrs_as_the_only_dependency() {
    let Some(outcome) = check_scaffold("operator", r#", features = ["operator"]"#, OPERATOR_MAIN)
    else {
        return;
    };
    assert!(
        outcome.compiled,
        "blueprint §9.3's operator sample does not compile for a crate whose only \
         dependency is `astrs`:\n{}",
        outcome.diagnostics
    );
}

#[test]
fn a_broken_sample_really_does_fail_to_compile() {
    // Without this, every assertion above could be passing because the
    // harness never actually observes a compiler.
    let Some(outcome) = check_scaffold("broken", "", BROKEN_MAIN) else {
        return;
    };
    assert!(
        !outcome.compiled,
        "a malformed `#[astrs(urn = ...)]` compiled — the harness is not \
         observing the compiler it thinks it is:\n{}",
        outcome.diagnostics
    );
    assert!(
        outcome.diagnostics.contains("std/"),
        "the derive's own URN diagnostic should be what rejected it:\n{}",
        outcome.diagnostics
    );
}
