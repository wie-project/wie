#!/usr/bin/env bash
# Build a PGO-optimized release binary of wie-cli (target/release/wie).
#
# Pipeline:
#   1. Instrumented release build (-Cprofile-generate).
#   2. Training runs, all headless:
#        - criterion wie-cpu hot-path benches (deep, deterministic coverage of
#          the emulator core: exec/dispatch/compile/mem);
#        - with --train real|both: real guest workloads via `wie trace`
#          (PE load + winapi + CPU) and `wie run --screenshot` (full runtime
#          loop incl. softmmu), skipped automatically when real_exes are
#          absent (e.g. CI checkouts — real_exes/ is not in git).
#   3. Toolchain llvm-profdata merges all .profraw files.
#   4. Final UNINSTRUMENTED release build (-Cprofile-use). Never benchmark or
#      ship the instrumented intermediate: its numbers include counter overhead.
#
# Usage:
#   ./scripts/build-release-pgo.sh                  # bench training only (CI-safe)
#   ./scripts/build-release-pgo.sh --train both     # + real workload traces
#
# Missing-profile warnings for wie-cli-only functions are expected when
# training ran benches only; those functions keep default optimization
# decisions. The perf-critical core (wie-cpu) is always covered.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

MODE="benches"
if [[ "${1:-}" == "--train" ]]; then
    MODE="${2:?--train benches|real|both}"
fi
case "$MODE" in benches|real|both) ;; *) echo "unknown --train mode: $MODE" >&2; exit 2 ;; esac

PROF_DIR="target/pgo-release-data"
PROFDATA="$PROF_DIR/merged.profdata"
BIN="$ROOT/target/release/wie"

LLVM_PROFDATA="$(find "$(rustc --print sysroot)/lib/rustlib" \
    -name llvm-profdata -type f 2>/dev/null | head -n 1)"
if [[ -z "$LLVM_PROFDATA" ]]; then
    echo "llvm-profdata not found: run 'rustup component add llvm-tools-preview'" >&2
    exit 2
fi

unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS

echo "== [1/4] instrumented release build =="
rm -rf "$PROF_DIR"
mkdir -p "$PROF_DIR"
export RUSTFLAGS="-Cprofile-generate=$PWD/$PROF_DIR"
cargo build -p wie-cli --release

echo
echo "== [2/4] training runs =="
# Benches: small sample counts — this is profile collection, not measurement.
cargo bench -p wie-cpu --bench jit_hot_paths -- \
    --sample-size 10 --warm-up-time 1 --measurement-time 1 \
    >"$PROF_DIR/train-benches.log" 2>&1 || true
echo "  ok: criterion hot-path benches"

if [[ "$MODE" != "benches" ]]; then
    train_trace() {
        local exe="$1"
        if [[ -f "$exe" ]]; then
            echo "  trace: $exe"
            timeout 300 "$BIN" trace --max-api 400 "$exe" \
                >"$PROF_DIR/train-trace-$(basename "$exe").log" 2>&1 || true
        else
            echo "  skip (not fetched): $exe"
        fi
    }
    train_trace real_exes/notepad.exe
    train_trace real_exes/2048.exe
    train_trace real_exes/7za.exe

    if [[ -f real_exes/notepad.exe ]]; then
        echo "  headless frame run: real_exes/notepad.exe"
        timeout 300 "$BIN" run --screenshot "$PROF_DIR/frame.bmp" real_exes/notepad.exe \
            >"$PROF_DIR/train-screenshot.log" 2>&1 || true
    fi
fi

shopt -s nullglob
PROFRAWS=("$PROF_DIR"/*.profraw)
if [[ ${#PROFRAWS[@]} -eq 0 ]]; then
    echo "no .profraw files produced — training failed?" >&2
    exit 1
fi

echo
echo "== [3/4] merging ${#PROFRAWS[@]} profile file(s) =="
"$LLVM_PROFDATA" merge -output="$PWD/$PROFDATA" "${PROFRAWS[@]}"
ls -lh "$PROFDATA"

echo
echo "== [4/4] final PGO release build =="
unset RUSTFLAGS
export RUSTFLAGS="-Cprofile-use=$PWD/$PROFDATA"
cargo build -p wie-cli --release
unset RUSTFLAGS

ls -lh "$BIN"
echo "done: $BIN"
