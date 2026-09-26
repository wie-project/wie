#!/usr/bin/env bash
# Wave 2 Step 3 acceptance capture (docs/implementation-plan.md).
# Runs gui_d3d9.exe in its continuous-present self-test mode (WIE_SELFTEST=2),
# injected via WIE_GUEST_ENV, through the production GUI adapter on a RELEASE
# build under WIE_RUNTIME_PROFILE=1, and ends the session with the SIGINT
# profile watchdog. The hard invariants are:
#
#   1. production wie run --gui capture path
#   2. capture_frames > 0 and capture_frames <= present_enqueued
#   3. present_ms > 0
#   4. handler_ms / present_enqueued <= 1 ms
#   5. process CPU is informational
#
# Usage: ./scripts/acceptance-wave2.sh [duration_secs=40]
#        WAVE2_BASELINE=1 also appends the report to docs/baselines/wave2-acceptance.txt
#
# Wall-clock numbers: idle machine, release build only.
set -euo pipefail

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
  make -C "$ROOT/micro-exes" out/gui_d3d9.exe >/dev/null
  if [[ ! -f "$PE" ]]; then
    echo "FAIL: make did not produce $PE" >&2
    exit 1
  fi
fi

# The report prints via tracing::error! / eprintln on HostInterrupt.
export RUST_LOG="error"
TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/wave2-acceptance.XXXXXX")"
OUT="$TMP_ROOT/output.txt"
BOTTLE="$TMP_ROOT/bottle"
mkdir -p "$BOTTLE"
trap 'rm -rf "$TMP_ROOT"' EXIT

echo "=== Wave 2 acceptance: gui_d3d9 WIE_SELFTEST=2, ${SECS}s capture ===" >&2
# Production `--gui` capture path must be the active one; the legacy
# in-handler raster path is only reached with WIE_CAPTURE_STREAM=0.
unset WIE_CAPTURE_STREAM

status=0
if command -v timeout >/dev/null 2>&1 && timeout --help 2>&1 | grep -q -- "--signal"; then
  set +e
  WIE_RUNTIME_PROFILE=1 WIE_GUEST_ENV="WIE_SELFTEST=2" \
    timeout --signal=INT --kill-after=5s "${SECS}s" \
    "$CLI" run --gui --root "$BOTTLE" "$PE" >"$OUT" 2>&1
  status=$?
  set -e
else
  set +e
  WIE_RUNTIME_PROFILE=1 WIE_GUEST_ENV="WIE_SELFTEST=2" \
    "$CLI" run --gui --root "$BOTTLE" "$PE" >"$OUT" 2>&1 &
  pid=$!
  ( sleep "$SECS"; kill -INT "$pid" 2>/dev/null || true ) &
  watchdog=$!
  wait "$pid" || status=$?
  kill "$watchdog" 2>/dev/null || true
  wait "$watchdog" 2>/dev/null || true
  set -e
fi

case "$status" in
  124|130)
    ;;
  *)
    echo "FAIL: unexpected emulator status $status (expected 124 or 130)" >&2
    tail -n 30 "$OUT" >&2 || true
    exit 1
    ;;
esac

report="$(grep -A200 "WIE_RUNTIME_PROFILE" "$OUT" || true)"
if [[ -z "$report" ]]; then
  echo "FAIL: no WIE_RUNTIME_PROFILE report in the capture (SIGINT→profile handoff)" >&2
  tail -n 30 "$OUT" >&2 || true
  exit 1
fi
echo "$report"

# ---- invariants ----
fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

frames_published="$(sed -n 's/.*frames_published=\([0-9]*\).*/\1/p' <<<"$report" | tail -1)"
handler_ms="$(sed -n 's/.*handler_ms=\([0-9.]*\).*/\1/p' <<<"$report" | tail -1)"
present_ms="$(sed -n 's/.*present_ms=\([0-9.]*\).*/\1/p' <<<"$report" | tail -1)"
present_enqueued="$(sed -n 's/.*present_enqueued=\([0-9]*\).*/\1/p' <<<"$report" | tail -1)"
capture_frames="$(sed -n 's/.*capture_frames=\([0-9]*\).*/\1/p' <<<"$report" | tail -1)"
cpu_pct="$(sed -n 's/.*cpu%≈\([0-9.]*\).*/\1/p' <<<"$report" | tail -1)"

[[ -n "$handler_ms" ]] || fail "handler_ms missing from profile"
[[ -n "$present_ms" ]] || fail "present_ms missing from profile"
[[ -n "$present_enqueued" ]] || fail "present_enqueued missing from profile"
[[ -n "$capture_frames" ]] || fail "capture_frames missing from profile"

(( present_enqueued > 0 )) ||
  fail "present_enqueued is zero — no D3D9 Present boundary was sampled"
(( capture_frames > 0 )) ||
  fail "capture_frames is zero — production capture thread published nothing"
(( capture_frames <= present_enqueued )) ||
  fail "capture_frames=${capture_frames} exceeds present_enqueued=${present_enqueued}"

if ! python3 -c "import sys; sys.exit(0 if $present_ms > 0.0 else 1)"; then
  fail "present_ms=${present_ms} — winit/wgpu consumed no frame"
fi

handler_per_present="$(python3 -c "print(f'{$handler_ms / $present_enqueued:.3f}')")"
if python3 -c "import sys; sys.exit(0 if $handler_ms / $present_enqueued <= 1.0 else 1)"; then
  echo "PASS: handler time ${handler_per_present} ms/Present ≤ 1 ms" >&2
else
  fail "handler time ${handler_per_present} ms/Present > 1 ms"
fi

echo "PASS: capture_frames=${capture_frames} <= present_enqueued=${present_enqueued}" >&2
echo "PASS: present_ms=${present_ms}" >&2
echo "INFO: frames_published=${frames_published:-0} (GDI/GL/DIB counter, not the capture denominator)" >&2
if [[ -n "$cpu_pct" ]]; then
  echo "INFO: process cpu%≈${cpu_pct}% (not a guest-thread acceptance gate)" >&2
fi

if [[ "${WAVE2_BASELINE:-0}" == "1" ]]; then
  mkdir -p "$OUTDIR"
  {
    printf '%s | %ss | present_enqueued=%s | capture_frames=%s | frames_published=%s | handler_ms/present=%s | present_ms=%s | cpu%%=%s | ' \
      "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
      "$SECS" \
      "$present_enqueued" \
      "$capture_frames" \
      "${frames_published:-0}" \
      "$handler_per_present" \
      "$present_ms" \
      "${cpu_pct:-unavailable}"
    tr '\n' ' ' <<<"$report" | sed 's/  */ /g'
    printf '\n'
  } >>"$BASELINE"
  echo "appended to $BASELINE" >&2
fi

exit 0
