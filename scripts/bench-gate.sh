#!/usr/bin/env bash
#
# scripts/bench-gate.sh -- turns the blueprint §20.4 criterion latency-ladder
# benches into a hard pass/fail gate.
#
# The bench binaries themselves never fail on a missed target: each of the
# six §20.4 rows (astrs.md §20.4) prints one
#   BENCH_GATE name=... p99_us=... target_us=... result=PASS|FAIL
# line and moves on -- see the header of any crates/*/benches/*.rs or
# benches/astrs-benches/benches/cold_start.rs for why (criterion custom
# output, never a panic on a miss). This script is what turns a FAIL line,
# or a missing one, into a nonzero exit code.
#
# ADVISORY, NOT A CI GATE: repo policy allows no CI workflow beyond the
# publish ones (astrs.md §20.3), so this never runs unattended -- run it
# locally, by hand, on an otherwise-quiet machine, before trusting its
# verdict. Wall-clock latency benchmarks are sensitive to machine load,
# thermal throttling, and whatever else is competing for the CPU/network/
# disk at the moment; a borderline FAIL is a prompt to re-run on a quiet
# machine before it is treated as a real regression.
#
# Usage: scripts/bench-gate.sh
#
# Exit codes:
#   0  all six §20.4 targets passed.
#   1  at least one target missed, or the suite did not produce exactly six
#      BENCH_GATE lines (a crashed bench and a bench that silently produced
#      no gate line both look like "did not pass" to this script, and are
#      both treated as a failure rather than silently ignored).

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

target_dir="${CARGO_TARGET_DIR:-$root/target}"

echo "==> Building astrs-cli and hello-timer once (release)."
echo "    Pre-building here -- and exporting ASTRS_BENCH_CLI/ASTRS_BENCH_NODE"
echo "    below -- is what lets the cold-start bench's own setup skip a"
echo "    nested \`cargo build\` entirely; see that bench file's header for"
echo "    why a nested build is not merely slower but can deadlock."
cargo build --release -p astrs-cli -p hello-timer

export ASTRS_BENCH_CLI="$target_dir/release/astrs"
export ASTRS_BENCH_NODE="$target_dir/release/hello-timer"
for bin in "$ASTRS_BENCH_CLI" "$ASTRS_BENCH_NODE"; do
    if [[ ! -x "$bin" ]]; then
        echo "bench-gate: expected a release binary at $bin" >&2
        echo "  (CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-<unset, defaulted to $root/target>})" >&2
        exit 1
    fi
done

# The five gate-bearing bench targets (astrs-shm's `handoff` alone emits two
# BENCH_GATE lines -- the SHM handoff and RTT rows both live in it -- so five
# targets cover the six §20.4 rows), plus astrs-wire's `frame_codec` and
# astrs-data's `kernels`: not §20.4 gates themselves (see each file's own
# header), included here so one invocation of this script also exercises the
# two SIMD-gating micro-benches named alongside the latency ladder. Only the
# five gate-bearing targets contribute to the six-line count checked below.
#
# `astrs-wire`'s other bench, `crc32c`, is deliberately NOT in this array: it
# is a finer-grained sweep over the same CRC-32C kernel `frame_codec` already
# isolates at two sizes, exists purely as SIMD-dispatch before/after evidence
# (see that file's own header), and its 14 size points would add roughly two
# more minutes to every run of this script for a target already covered.
# Run it directly when that evidence is needed:
#   cargo bench -p astrs-wire --bench crc32c
#
# Package-scoped on purpose, never --workspace: it isolates a failing gate
# to one crate immediately, and never contends with another agent's or
# developer's concurrent cargo invocation over a workspace-wide build lock.
benches=(
    "astrs-shm:handoff"
    "astrs-transport:mux_loopback"
    "astrs-scheduler:timer_jitter"
    "astrs-rtps:loopback_pubsub"
    "astrs-wire:frame_codec"
    "astrs-data:kernels"
    "astrs-benches:cold_start"
)

output="$(mktemp -t astrs-bench-gate.XXXXXX)"
trap 'rm -f "$output"' EXIT

for entry in "${benches[@]}"; do
    package="${entry%%:*}"
    bench="${entry##*:}"
    echo
    echo "==> cargo bench -p $package --bench $bench"
    # A crashing bench must not abort this script before the summary below
    # gets to say so plainly: a missing BENCH_GATE line reports the same
    # way a target miss does.
    cargo bench -p "$package" --bench "$bench" -- --quiet | tee -a "$output" || true
done

echo
echo "==> §20.4 gate lines:"
grep '^BENCH_GATE ' "$output" || echo "  (none)"

expected=6
gate_lines="$(grep -c '^BENCH_GATE ' "$output" || true)"
fail_lines="$(grep -c '^BENCH_GATE .*result=FAIL$' "$output" || true)"

status=0
echo
if [[ "$gate_lines" -ne "$expected" ]]; then
    echo "bench-gate: expected exactly $expected BENCH_GATE lines, found $gate_lines" >&2
    echo "  (a bench crashed, or produced no gate line -- see the log above)" >&2
    status=1
fi
if [[ "$fail_lines" -ne 0 ]]; then
    echo "bench-gate: $fail_lines of $gate_lines §20.4 target(s) missed" >&2
    status=1
fi
if [[ "$status" -eq 0 ]]; then
    echo "bench-gate: all $expected §20.4 targets passed."
fi

exit "$status"
