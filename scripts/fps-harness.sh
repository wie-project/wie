#!/usr/bin/env bash
# End-to-end FPS/present harness (Wave 0b).
#
# Runs the frame-heavy GUI micro exes headless on a RELEASE build with
# `WIE_RUNTIME_PROFILE=1`, extracts the frame/present/JIT counters from the
# profile report, and appends one line per workload to
# `docs/baselines/fps.txt` (with a UTC timestamp + host note).
#
# Usage: ./scripts/fps-harness.sh [exe ...]        (default: gui_blit gui_d3d9)
#        FPS_BASELINE=1  also update docs/baselines/fps.txt
#
# All numbers are wall-clock dependent: run on an idle machine, release build
# only, and compare same-machine baselines only.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CLI="${CLI:-$ROOT/target/release/wie}"
OUT="$ROOT/micro-exes/out"
BASELINE="$ROOT/docs/baselines/fps.txt"
EXES=("$@")
[[ ${#EXES[@]} -eq 0 ]] && EXES=(gui_blit gui_d3d9)

if [[ ! -x "$CLI" ]]; then
  echo "building wie (release)…"
  cargo build -p wie-cli --release --manifest-path "$ROOT/Cargo.toml"
fi
make -C "$ROOT/micro-exes" all >/dev/null

export WIE_RUNTIME_PROFILE=1
# The profile report prints via `tracing::error!` in the profiled paths.
export RUST_LOG="error"

for exe in "${EXES[@]}"; do
  pe="$OUT/$exe.exe"
  [[ -f "$pe" ]] || { echo "missing $pe (make -C micro-exes)"; exit 1; }
  echo "=== $exe ==="
  report="$("$CLI" run "$pe" 2>&1 | grep -A100 "WIE_RUNTIME_PROFILE" || true)"
  echo "$report"
  frames="$(sed -n 's/.*frames_published=\([0-9]*\).*/\1/p' <<<"$report" | tail -1)"
  if [[ "${FPS_BASELINE:-0}" == "1" ]]; then
    {
      printf '%s | %s | ' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$exe"
      tr '\n' ' ' <<<"$report" | sed 's/  */ /g'
      printf '\n'
    } >>"$BASELINE"
    echo "appended to $BASELINE"
  fi
done
