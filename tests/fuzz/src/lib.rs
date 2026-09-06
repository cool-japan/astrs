//! `astrs-fuzz`: the pure-Rust deep-fuzz estate for AstRS's four attack
//! surfaces (blueprint §15).
//!
//! `cargo-fuzz`/libfuzzer links a C++ runtime and is excluded outright by
//! the Pure Rust policy (blueprint §3.1, §18.1) — every crate's own `cargo
//! test` path stays on `proptest` for that reason (see e.g. `astrs-cdr`'s
//! crate docs). This crate is the pure-Rust replacement for the
//! deep-coverage half of that job: a structured generator built on an
//! in-crate PRNG (no `rand` — not a workspace dependency), run far past
//! what a property test's default case count would spend, against a
//! regression corpus committed to the repository so a crash, once fixed,
//! can never resurface silently.
//!
//! # The four surfaces
//!
//! | Module | Target crate | Entry point |
//! |---|---|---|
//! | [`surfaces::wire`] | `astrs-wire` | `decode_frame` / `decode_frame_prefix` |
//! | [`surfaces::data`] | `astrs-data` | `ipc::decode_payload` / `ipc::IpcStreamReader` |
//! | [`surfaces::cdr`] | `astrs-cdr` | `from_bytes` / `ParameterList::decode` |
//! | [`surfaces::rtps`] | `astrs-rtps` | `messages::Message::decode` |
//!
//! # Two lanes, one estate
//!
//! - **The default lane** (`cargo test -p astrs-fuzz` / `cargo nextest run
//!   -p astrs-fuzz`): `ASTRS_FUZZ_ITERS` defaults to 10 000 cases per
//!   surface — fast enough to stay in the ordinary test suite — plus an
//!   **unconditional** replay of every file under `corpus/<surface>/`, so a
//!   fixed regression is re-checked on every run, gated on nothing.
//! - **The nightly lane** (`scripts/fuzz-nightly.sh`, from the repository
//!   root): the same four harnesses at `ASTRS_FUZZ_ITERS=5000000`.
//!
//! # Adding a regression case
//!
//! [`support::driver::fuzz_surface`] never writes to the repository — a
//! failure is minimized in memory and printed as hex to stderr. Turning
//! that into a permanent regression is a deliberate step: fix the
//! underlying bug first, then paste the printed hex into a new file under
//! `corpus/<surface>/` (`xxd -r -p` turns hex back into bytes), so the file
//! starts life as a *passing* regression test.

pub mod support;
pub mod surfaces;
