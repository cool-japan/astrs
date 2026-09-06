#!/usr/bin/env bash
#
# scripts/ci-local.sh -- the per-change gate (astrs.md §20.3, §20.3's own
# words: "repo policy allows no CI workflows beyond the publish ones, so
# `scripts/ci-local.sh` is the per-change gate").
#
# This script is a thin wrapper: `cargo xtask preflight` (xtask/src/
# preflight.rs) is what actually runs every §20 quality gate -- structural
# checks (layer-lint, schema --check, the file-size audit,
# no-inline-version-pins) first, then `cargo fmt --check` and `cargo deny
# check bans`, then the *-sys sweep, then the wire-protocol freeze, then the
# expensive workspace-wide steps (clippy, nextest, doctests, docs) -- in one
# ordered pass, reporting pass/fail per step rather than stopping at the
# first failure. See that module's doc comment for the full step order and
# rationale.
#
# `cargo publish --dry-run` per publishable crate is release-day-only and
# NOT part of the default run here (it is slow, hits the crates.io index,
# and every crate it dry-runs must already resolve against what is
# currently published) -- pass --publish-dry-run to include it, exactly as
# `cargo xtask preflight` itself does.
#
# Usage: scripts/ci-local.sh [--publish-dry-run]
#
# Exit codes:
#   0  every gate passed (or, for the publish dry-run step, was skipped).
#   1  at least one gate failed -- see the per-step [PASS]/[FAIL]/[SKIP]
#      summary `cargo xtask preflight` prints for exactly which one(s).

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

echo "==> cargo xtask preflight $*"
exec cargo run --quiet -p xtask -- preflight "$@"
