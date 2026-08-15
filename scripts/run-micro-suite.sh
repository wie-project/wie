#!/usr/bin/env bash
# Run micro-EXE test gates by category. Exit non-zero on first failure.
# Usage: ./run-micro-suite.sh [category]
#   category: all (default), cpp_exes, seh_exes, dll_tests, console_tests, pthread_tests
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CLI="${CLI:-$ROOT/target/release/wie}"
CPU="${WIE_CPU:-jit}"
CATEGORY="${1:-all}"

# The per-run summary (path, events, exit) is a `wie::commands::run`-target
# debug log; show it by default so the suite reports what each exe did.
# (Note: EnvFilter matches string prefixes, so `wie` alone would also enable
# `wie_cpu`/`wiegui`/`wie_runtime` — the full target path keeps the noise out.)
export RUST_LOG="${RUST_LOG:-wie::commands::run=debug}"

if [[ ! -x "$CLI" ]]; then
  echo "building wie (release)…"
  cargo build -p wie-cli --release --manifest-path "$ROOT/Cargo.toml"
fi

export WIE_CPU="$CPU"
echo "=== micro-suite: ${CATEGORY} (WIE_CPU=$WIE_CPU) ==="

run_one() {
  local pe="$1"
  shift
  echo "--- run $pe $* ---"
  "$CLI" run "$pe" "$@"
}

run_bottle_n2() {
  local pe="$1"
  local root="$2"
  shift 2
  run_one "$pe" --root "$root" "$@"
}

# Build only the requested category
make -C "$ROOT/micro-exes" "${CATEGORY}"

OUT="$ROOT/micro-exes/out"

if [[ "$CATEGORY" == "all" ]]; then
  # CPU string-instruction gates. `rep_lengths` sweeps REP MOVS/STOS across
  # lengths 1..70 and guards the JIT inline-REP tail handling.
  run_one "$OUT/cpu_string.exe"
  run_one "$OUT/rep_lengths.exe"

  run_one "$OUT/cpp_throw.exe"
  run_one "$OUT/cpp_dtor.exe"
  run_one "$OUT/cpp_multi_catch.exe"
  run_one "$OUT/cpp_types.exe"
  run_one "$OUT/cpp_threads.exe"

  # DLL tests load their sibling *_funcs.dll files by bare name, so the whole
  # out/ folder is named explicitly with --app-dir (the default `wie run`
  # staging copies only the exe itself).
  for exe in "$OUT"/dll_*.exe; do
    [ -f "$exe" ] && run_one "$exe" --app-dir "$OUT"
  done

  # Console output tests (non-interactive: no stdin needed)
  run_one "$OUT/console_cells.exe"

  # CRT convergence gate (srand/rand constants, UCRT coverage)
  run_one "$OUT/rand_test.exe"
  run_one "$OUT/ucrt_coverage.exe"

  # Pthread tests via libwinpthread-1.dll
  run_one "$OUT/pt_basic.exe"
  run_one "$OUT/pt_cond.exe"

  # CreateProcess: stage child + parent in one bottle (child embeds the
  # compile-time path C:\child_proc.exe), parent waits + checks exit 42.
  SPAWN_ROOT="$(mktemp -d)"
  mkdir -p "$SPAWN_ROOT/drive_c"
  cp "$OUT/child_proc.exe" "$SPAWN_ROOT/drive_c/child_proc.exe"
  run_bottle_n2 "$OUT/spawn_child.exe" "$SPAWN_ROOT"
  rm -rf "$SPAWN_ROOT"

  # Fast-sync tail: failed MultiByteToWideChar -> GetLastError must observe
  # ERROR_INSUFFICIENT_BUFFER (exit 0 on success).
  run_one "$OUT/mbwc_lasterror.exe"
fi

if [[ "$CATEGORY" == "console_tests" ]]; then
  run_one "$OUT/console_cells.exe"
fi

if [[ "$CATEGORY" == "pthread_tests" ]]; then
  run_one "$OUT/pt_basic.exe"
  run_one "$OUT/pt_cond.exe"
fi

if [[ "$CATEGORY" == "cpp_exes" ]]; then
  run_one "$OUT/cpp_throw.exe"
  run_one "$OUT/cpp_dtor.exe"
  run_one "$OUT/cpp_multi_catch.exe"
  run_one "$OUT/cpp_types.exe"
  run_one "$OUT/cpp_threads.exe"
fi

if [[ "$CATEGORY" == "seh_exes" ]]; then
  run_one "$OUT/seh_access_violation.exe"
  run_one "$OUT/seh_div_zero.exe"
fi

if [[ "$CATEGORY" == "dll_tests" ]]; then
  # Same --app-dir staging as the `all` category: the DLL micros load their
  # sibling *_funcs.dll files by bare name.
  for exe in "$OUT"/dll_*.exe; do
    [ -f "$exe" ] && run_one "$exe" --app-dir "$OUT"
  done
fi

echo "=== micro-suite (${CATEGORY}): ok ==="
