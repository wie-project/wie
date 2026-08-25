#!/usr/bin/env bash
# PGO A/B benchmark: does Profile-Guided Optimization speed up the wie-cpu
# hot-path benches (exec/compile/mem)?
#
# Pipeline (each phase is a full rebuild because RUSTFLAGS changes):
#   1. baseline — plain release-profile build, criterion results saved as
#      the named baseline "no-pgo".
#   2. train    — instrumented build (-Cprofile-generate); one bench run
#      emits .profraw files. Numbers here are meaningless (instrumentation
#      overhead) and are discarded.
#   3. merge    — toolchain llvm-profdata merges raw profiles.
#   4. pgo      — rebuild with -Cprofile-use (uninstrumented!) and re-run,
#      printing per-case change % against the saved "no-pgo" baseline.
#
# All artifacts live under target/pgo-bench and target/pgo-data, so the
# normal target/ directory and criterion baselines are untouched.
#
# Usage:
#   ./scripts/pgo-bench.sh            # full pipeline
#   ./scripts/pgo-bench.sh summarize  # just re-print the comparison table
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

PROF_DIR="target/pgo-data"
PROFDATA="$PROF_DIR/merged.profdata"
TD="target/pgo-bench"
BASELINE_LOG="$PROF_DIR/baseline.log"
PGO_LOG="$PROF_DIR/pgo.log"

BENCH=(cargo bench -p wie-cpu --bench jit_hot_paths)
# Consistent methodology across both measured runs; small enough to finish
# quickly, large enough for criterion's resampled medians to be meaningful.
FAST=(--sample-size 30 --warm-up-time 1 --measurement-time 3)

LLVM_PROFDATA="$(find "$(rustc --print sysroot)/lib/rustlib" \
    -name llvm-profdata -type f 2>/dev/null | head -n 1)"
if [[ -z "$LLVM_PROFDATA" ]]; then
    echo "llvm-profdata not found: run 'rustup component add llvm-tools-preview'" >&2
    exit 2
fi

summarize() {
    echo
    echo "== PGO vs no-PGO (median change per case, negative = faster) =="
    # Criterion emits the change either inline ("change: [a% b% c%]") for
    # plain-time cases or split across lines ("change:" then "time: [a% b%
    # c%]") when throughput is printed. Track the case name from
    # "Benchmarking <case>:" lines and grab the first %-triple after each
    # "change:" marker; its middle value is the median delta.
    awk '
        /^Benchmarking [a-z_0-9\/]+: / {
            name = $2
            sub(/:$/, "", name)
        }
        /change:/ {saw = 1}
        saw && match($0, /\[[-+0-9.]+% [-+0-9.]+% [-+0-9.]+%\]/) {
            split(substr($0, RSTART + 1, RLENGTH - 2), pct, / +/)
            gsub(/%/, "", pct[2])
            printf "  %-38s %+9s%%\n", name, pct[2]
            saw = 0
        }
    ' "$PGO_LOG"
}

if [[ "${1:-}" == "summarize" ]]; then
    summarize
    exit 0
fi

mkdir -p "$PROF_DIR"
unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS

echo "== [1/4] baseline build + save-baseline 'no-pgo' =="
CARGO_TARGET_DIR="$PWD/$TD" "${BENCH[@]}" -- --save-baseline no-pgo "${FAST[@]}" \
    2>&1 | tee "$BASELINE_LOG"

echo
echo "== [2/4] instrumented training run (numbers discarded) =="
export RUSTFLAGS="-Cprofile-generate=$PWD/$PROF_DIR"
rm -f "$PROF_DIR"/*.profraw
CARGO_TARGET_DIR="$PWD/$TD" "${BENCH[@]}" -- "${FAST[@]}" \
    >"$PROF_DIR/train.log" 2>&1 || true
unset RUSTFLAGS
PROFRAW_COUNT="$(find "$PROF_DIR" -name '*.profraw' | wc -l | tr -d ' ')"
if [[ "$PROFRAW_COUNT" -eq 0 ]]; then
    echo "no .profraw files produced — instrumentation failed?" >&2
    tail -20 "$PROF_DIR/train.log" >&2
    exit 1
fi
echo "collected $PROFRAW_COUNT .profraw file(s)"

echo
echo "== [3/4] merging profiles =="
"$LLVM_PROFDATA" merge -output="$PWD/$PROFDATA" "$PROF_DIR"/*.profraw
ls -lh "$PROFDATA"

echo
echo "== [4/4] PGO build (-Cprofile-use) + comparison =="
export RUSTFLAGS="-Cprofile-use=$PWD/$PROFDATA"
CARGO_TARGET_DIR="$PWD/$TD" "${BENCH[@]}" -- --baseline no-pgo "${FAST[@]}" \
    2>&1 | tee "$PGO_LOG"
unset RUSTFLAGS

summarize
