#!/usr/bin/env bash
# Wave 2 Step 3 acceptance capture (docs/implementation-plan.md).
# Runs gui_d3d9.exe in its continuous-present self-test mode (WIE_SELFTEST=2),
# injected via WIE_GUEST_ENV, headless on a RELEASE build under
# WIE_RUNTIME_PROFILE=1, ends the session with the SIGINT profile watchdog,
# and checks the Wave 2 invariants from the profile report:
#
#   1. emu thread ≤ 1 ms/frame   — emu_ms / frames_published
#   2. commit_ms / capture_ms    — the render-thread share is reported
#                                  separately from guest time
#   3. guest CPU ≳ 90%           — cpu%≈ from the report
#
# Usage: ./scripts/acceptance-wave2.sh [duration_secs=40]
#        WAVE2_BASELINE=1 also appends the report to docs/baselines/wave2-acceptance.txt
#
# Wall-clock numbers: idle machine, release build only.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." pwd)"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CLI="${CLI:-$ROOT/target/release/wie}"
PE="$ROOT/micro-exes/out/gui_d3d9.exe"
SECS="${1:-40}"
OUTDIR="$ROOT/docs/baselines"
BASELINE="$OUTDIR/wave2-acceptance.txt"

if ! [[ "$SECS" =~ ^[0-9]+$ ]]; then
  echo "usage: $0 [duration_secs=40]" >&2
  exit 2
fi

if [[ ! -x "$CLI" ]]; then
  echo "building wie (release)…"
  cargo build -p wie-cli --release --manifest-path "$ROOT/Cargo.toml"
fi
if [[ ! -f "$PE" ]]; then
  echo "building micro-exes…"
  make -C "$ROOT/micro-exes" gui_d3d9 >/dev/null
fi

# The report prints via tracing::error! / eprintln on HostInterrupt.
export RUST_LOG="error"
OUT="$(mktemp /tmp/wave2-acceptance.XXXXXX.txt)"
trap 'rm -f "$OUT"' EXIT

echo "=== Wave 2 acceptance: gui_d3d9 WIE_SELFTEST=2, ${SECS}s capture ===" >&2
status=0
if command -v timeout >/dev/null 2>&1 && timeout --help 2>&1 | grep -q -- "--signal"; then
  set +e
  WIE_RUNTIME_PROFILE=1 WIE_GUEST_ENV="WIE_SELFTEST=2" \
    timeout --signal=INT --kill-after=5s "${SECS}s" "$CLI" run "$PE" >"$OUT" 2>&1
  status=$?
  set -e
else
  set +e
  WIE_RUNTIME_PROFILE=1 WIE_GUEST_ENV="WIE_SELFTEST=2" \
    "$CLI" run "$PE" >"$OUT" 2>&1 &
  pid=$!
  ( sleep "$SECS"; kill -INT "$pid" 2>/dev/null || true ) &
  watchdog=$!
  wait "$pid" || status=$?
  kill "$watchdog" 2>/dev/null || true
  wait "$watchdog" 2>/dev/null || true
  set -e
fi
if [[ $status -ne 0 && $status -ne 130 && $status -ne 124 ]]; then
  echo "warning: emulator exited with status $status (expected 0/124/130)" >&2
fi

report="$(grep -A100 "WIE_RUNTIME_PROFILE" "$OUT" || true)"
if [[ -z "$report" ]]; then
  echo "FAIL: no WIE_RUNTIME_PROFILE report in the capture (SIGINT→profile handoff)" >&2
  tail -n 30 "$OUT" >&2 || true
  exit 1
fi
echo "$report"

# ---- invariants ----
fail=0
frames="$(sed -n 's/.*frames_published=\([0-9]*\).*/\1/p' <<<"$report" | tail -1)"
emu_ms="$(sed -n 's/.*emu_ms=\([0-9.]*\).*/\1/p' <<<"$report" | tail -1)"
cpu_pct="$(sed -n 's/.*cpu%≈\([0-9.]*\).*/\1/p' <<<"$report" | tail -1)"
capture_frames="$(sed -n 's/.*capture_frames=\([0-9]*\).*/\1/p' <<<"$report" | tail -1)"
commit_frames="$(sed -n 's/.*commit_frames=\([0-9]*\).*/\1/p' <<<"$report" | tail -1)"

if [[ -z "$frames" || "$frames" -eq 0 ]]; then
  echo "FAIL: frames_published missing/zero — no continuous frame stream" >&2
  fail=1
  frames="${frames:-0}"
fi

emu_per_frame=""
if [[ -n "$emu_ms" && "$frames" -gt 0 ]]; then
  emu_per_frame="$(python3 -c "print(f'{$emu_ms / $frames:.3f}')")"
  ok="$(python3 -c "print(1 if $emu_ms / $frames <= 1.0 else 0)")"
  if [[ "$ok" != "1" ]]; then
    echo "FAIL: emu thread ${emu_per_frame} ms/frame > 1 ms/frame" >&2
    fail=1
  else
    echo "PASS: emu thread ${emu_per_frame} ms/frame ≤ 1 ms/frame" >&2
  fi
fi

if [[ -z "$capture_frames" || "$capture_frames" -eq 0 ]] && [[ -z "$commit_frames" || "$commit_frames" -eq 0 ]]; then
  echo "FAIL: neither capture_frames nor commit_frames nonzero — the render \
thread published nothing (render share not separated from guest time)" >&2
  fail=1
else
  echo "PASS: render-thread counters present (capture_frames=${capture_frames:-0} \
commit_frames=${commit_frames:-0})" >&2
fi

if [[ -n "$cpu_pct" ]]; then
  ok="$(python3 -c "print(1 if $cpu_pct >= 90.0 else 0)")"
  if [[ "$ok" != "1" ]]; then
    echo "WARN: guest cpu%≈${cpu_pct}% < 90% (idle-machine requirement; not a hard failure)" >&2
  else
    echo "PASS: guest cpu%≈${cpu_pct}% ≥ 90%" >&2
  fi
else
  echo "WARN: cpu%≈ not found in the report" >&2
fi

if [[ "${WAVE2_BASELINE:-0}" == "1" ]]; then
  {
    printf '%s | %ss | frames=%s | emu_ms/frame=%s | cpu%%=%s | capture_frames=%s | commit_frames=%s | ' \
      "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$SECS" "$frames" "${emu_per_frame:-?}" "${cpu_pct:-?}" "${capture_frames:-0}" "${commit_frames:-0}"
    tr '\n' ' ' <<<"$report" | sed 's/  */ /g'
    printf '\n'
  } >>"$BASELINE"
  echo "appended to $BASELINE" >&2
fi

exit "$fail"
