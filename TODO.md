# AstRS — TODO

**0.1.0 — released 2026-09-06.**

What shipped is described in [`README.md`](README.md) — its *Feature
status* section is the authoritative account of what is complete, what is
deliberately partial and what is out of scope — and in
[`CHANGELOG.md`](CHANGELOG.md). This file carries only what is still to be
done. The wave-by-wave build history that used to live here is in the git
history (`git show <commit>:TODO.md`); nothing below depends on it.

---

## Before `cargo publish`

- [ ] `scripts/ci-local.sh --publish-dry-run` (or `cargo xtask preflight
      --publish-dry-run`) at least once. It hits the live crates.io index,
      so it is a release-day step rather than a per-pass one.
- [ ] Publish in the topological order `cargo xtask preflight` already
      computes (`xtask::workspace::topological_order`). `xtask` has no
      `--execute` mode by design — the real `cargo publish` is run by hand.
- [ ] Confirm the one remaining `[patch.crates-io]` entry does not affect
      what downstream consumers resolve: `alloca = { path =
      "patches/alloca-0.4.0" }` is a Pure Rust reimplementation substituted
      for an upstream crate that carries a `cc` build-dependency, and it
      reaches this tree only through `criterion`'s dev-dependencies. The
      dry run is what settles it.
- [ ] Branch/version decision, at commit time: `main` takes only the
      "Availability of 0.1.0" commit, and the next `0.x.y` branch is cut
      immediately after for post-release work.
- [ ] `git stash@{0}` ("wip: idl leading_comment fix + regen") is still on
      the stack. One piece of it (the `linkify_bare_urls` codegen fix) was
      already merged; the rest is a superseded integration narrative
      against an older tree. `git stash show -p stash@{0}`, then drop it.

## Known gaps — post-0.1.0

Each of these is named in README's *Feature status* as a shipped
limitation; this is the work side of the same list.

- [ ] **Windows daemon and cross-host transport.** `cargo check -p
      astrs-daemon --target x86_64-pc-windows-msvc` fails in two transitive
      crates, not in `astrs-daemon` itself:
      `crates/astrs-transport/src/backend/uds.rs` (Unix-domain sockets,
      chosen for kernel-vouched uid/gid/pid peer credentials and implicit
      fd-passing — Windows has neither) and
      `crates/astrs-telemetry/src/sampler/mod.rs` (`rustix::process`, which
      is `#[cfg(not(windows))]` upstream). A real Windows leg means a
      named-pipe or loopback-TCP backend plus a peer-identity story that
      does not rest on uid/gid/pid, and it needs `windows-sys`'s
      `Win32_System_Threading` feature enabled, which the workspace
      currently does not. `astrs-shm` already has a real `os/windows.rs`
      to model the shape of the port on.
- [ ] **QUIC listener.** QUIC dials, frames and tests clean, but ships
      behind the non-default `quic` feature because `oxiquic 0.2.1`'s
      server constructors (`listen`, `ServerEndpoint::bind`) take `rustls`
      types the facade does not re-export. `quic:` addresses dial over TCP
      with identical framing until then;
      `backend::quic::serve_connection` is the seam a host with its own
      `rustls` passes an accepted connection through, and
      `backend::effective_plane` reports which plane a dial really uses.
      Revisit when `oxiquic` exposes a facade-level server constructor.
- [ ] **TLS/PSK derivation from the cluster auth token.** `oxiquic 0.2.1`
      exposes no TLS 1.3 external-PSK API (certificate-based `rustls`
      only). The documented degradation is in place instead — QUIC
      authenticates the channel, the AstRS `Hello` authenticates the
      cluster (constant-time 32-byte token compare in `astrs-wire`'s
      `Acceptor`, `RefusalReason::BadAuth` before any route opens).
- [ ] **`astrs replay` into a running dataflow.** `--into <manifest.yml>`,
      the offline manifest-rewrite form, is the only one that exists
      (`bins/astrs-cli/src/command/replay.rs`); a live cutover has no
      implementation.
- [ ] **Make the conformance precondition mechanical or self-healing.**
      `cargo test -p astrs-conformance` run on its own fails with
      `MissingBinary` until the example binaries exist, because no example
      crate carries a `tests/` directory and cargo therefore never builds
      their `[[bin]]` targets. Two cargo-native fixes: give each example
      package a `tests/` target (a real test that the built node binary
      refuses a launch outside a dataflow would earn its keep), or have
      `astrs-conformance::paths::binary` build on demand — the second also
      closes the staleness hole, where a *present but stale* binary is
      neither rebuilt nor detected. Whichever is chosen, encode it in
      `cargo xtask preflight`. Not a live failure: every `--workspace`
      nextest run and `xtask preflight` build the whole tree first.
- [ ] **Deepen `astrs-migrate` and `astrs-recording`.** Both are fully
      implemented and green; both are still thinner than the scope
      the crate catalog described as a floor, not a target.
- [ ] **Python bridge**, as a separate out-of-tree repo. PyO3 links
      `libpython`, so it can never be a member of this workspace under the
      Pure-Rust-absolute rule — it bridges into `astrs` the way any other
      external consumer does.
- [ ] **Cross-stack ROS 2 validation** (live Fast-DDS / CycloneDDS
      interop), likewise a separate out-of-repo project: the in-tree RTPS
      bench measures loopback self-interop between two real AstRS
      participants, which is the in-repo evidence standard, not a live
      third-party stack.

## Watch items

- [ ] **File-size ceiling.** `crates/astrs-rtps/src/behavior/writer.rs` is
      at 1,948 lines — 52 lines of headroom under the 2,000-line rule and
      now the closest file in the workspace, ahead of
      `crates/astrs-transport/src/mux/state.rs` at 1,926. The gate is
      `xtask preflight`'s own audit (`xtask::preflight::MAX_FILE_LINES`);
      `rslines` is not installed on this machine and is not needed for it.
      No split is required yet — split with `splitrs` when one crosses.
- [ ] **`multiple-versions = "warn"`, not `"deny"`** in `deny.toml`. The
      retained external list pulls `syn` 1.x/2.x/3.x transitively. Revisit
      when the tree can carry the stricter setting.
- [ ] **No dedicated flake hunt.** No flake has been observed in any full
      `--workspace` run, in either feature set, but repeated-stress runs
      beyond those single clean passes have never been done. The real-time
      end-to-end tests measure wall-clock budgets over loopback sockets, so
      they are the ones to point a stress run at first.

## Doc debt

- [ ] **The "blueprint §N" citation sweep — code comments only.**
      `astrs.md`, the engineering blueprint this workspace was built from,
      was retired before 0.1.0 shipped (see README's closing note). Every
      document that ships with the workspace has been swept: no markdown
      file still cites a section number, every publishable crate's
      `README.md` points at the workspace README or at the crate's own
      docs.rs page instead, and every cross-crate link in those READMEs is
      an absolute URL so it resolves on crates.io as well as on GitHub.
      What remains is **4,424 citations across 866 `.rs` files** — doc
      comments and inline comments that still say "blueprint §N". They
      render into the rustdoc on docs.rs, so they are public; the section
      numbers resolve nowhere, though every sentence they sit in stands on
      its own. Rewriting each to name the rustdoc or README section that
      now carries the same content is a per-file judgement call,
      deliberately not done mechanically.
