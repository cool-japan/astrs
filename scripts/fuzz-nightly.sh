#!/usr/bin/env bash
#
# scripts/fuzz-nightly.sh -- the nightly lane of the blueprint §15 pure-Rust
# deep-fuzz estate: the same four `astrs-fuzz` harnesses the default test
# suite runs at 10 000 iterations each, run here at 5 000 000.
#
# cargo-fuzz/libfuzzer is excluded outright by the Pure Rust policy (§18.1)
# -- it links a C++ runtime -- so this is env-gated structured fuzzing
# instead: an in-crate PRNG, structure-aware mutation of valid encodings, and
# a committed regression corpus under tests/fuzz/corpus/<surface>/ that every
# `cargo test -p astrs-fuzz` run (this script included) replays
# unconditionally.
#
# ADVISORY, NOT A CI GATE: repo policy allows no CI workflow beyond the
# publish ones (astrs.md §20.3), so this never runs unattended -- run it
# locally, by hand, whenever the four attack-surface decoders (astrs-wire's
# frame codec, astrs-data's Arrow IPC reader, astrs-cdr's reader,
# astrs-rtps's submessage parser) have changed, or on whatever cadence you
# consider "nightly" for this repository.
#
# A failure here is a decoder panicking on adversarial input, printed with
# a minimized reproduction (see astrs-fuzz's own crate docs) -- fix the
# underlying bug in the owning crate, then commit the minimized case to
# tests/fuzz/corpus/<surface>/ so it can never regress silently again.
#
# Usage: scripts/fuzz-nightly.sh
#
# Exit codes:
#   0  all four harnesses ran clean at ASTRS_FUZZ_ITERS=5000000.
#   1  at least one harness found a panic, or the run otherwise failed.

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

export ASTRS_FUZZ_ITERS="${ASTRS_FUZZ_ITERS:-5000000}"

# Package-scoped and sequential on purpose, never --workspace and never
# parallel: this isolates a failing harness to its own line of output
# immediately, and never contends with another agent's or developer's
# concurrent cargo invocation over a workspace-wide build lock.
harnesses=(
    "wire_fuzz:wire_frame_decoder_survives_structured_fuzzing"
    "data_fuzz:arrow_ipc_reader_survives_structured_fuzzing"
    "cdr_fuzz:cdr_reader_survives_structured_fuzzing"
    "rtps_fuzz:rtps_submessage_parser_survives_structured_fuzzing"
)

echo "==> astrs-fuzz nightly lane: ASTRS_FUZZ_ITERS=$ASTRS_FUZZ_ITERS"

status=0
for entry in "${harnesses[@]}"; do
    test_binary="${entry%%:*}"
    test_name="${entry##*:}"
    echo
    echo "==> cargo test -p astrs-fuzz --test $test_binary -- --exact $test_name"
    if ! cargo test -p astrs-fuzz --test "$test_binary" --release -- --exact "$test_name"; then
        echo "fuzz-nightly: $test_binary::$test_name FAILED -- see the panic and its minimized" >&2
        echo "  reproduction above; commit it under tests/fuzz/corpus/<surface>/ once fixed" >&2
        status=1
    fi
done

# The corpus replay is unconditional in every default-lane test already, but
# running it explicitly here as well gives it its own clearly labeled line
# in a nightly log, separate from the 5 000 000-iteration lines above.
# `--release`, matching the harness runs above: the two would otherwise
# build the same four test binaries twice, once per profile.
echo
echo "==> cargo test -p astrs-fuzz --release --test wire_fuzz --test data_fuzz --test cdr_fuzz --test rtps_fuzz -- regression_corpus_replays_clean"
if ! cargo test -p astrs-fuzz --release --test wire_fuzz --test data_fuzz --test cdr_fuzz --test rtps_fuzz -- regression_corpus_replays_clean; then
    echo "fuzz-nightly: a committed regression case failed to replay clean" >&2
    status=1
fi

echo
if [[ "$status" -eq 0 ]]; then
    echo "fuzz-nightly: all four harnesses ran clean at $ASTRS_FUZZ_ITERS iterations."
else
    echo "fuzz-nightly: at least one harness or corpus replay failed -- see above." >&2
fi

exit "$status"
