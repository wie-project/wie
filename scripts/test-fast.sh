#!/usr/bin/env bash
# Fast dev-loop test lane: the whole workspace suite MINUS the two slow
# guest-spawning groups (the GUI_SUITE_LOCK-serialized `micro_gui_window`
# suite, and the rest of the `wie-runtime` real-PE integration tests).
#
# This is NOT the pre-PR gate. `scripts/check.sh` remains authoritative:
# it runs fmt, clippy, the FULL nextest suite and the micro-suite. Run this
# while iterating, then run check.sh before opening a PR.
#
# Usage: ./scripts/test-fast.sh [extra nextest args...]
#   e.g. ./scripts/test-fast.sh -p wie-cpu
#        ./scripts/test-fast.sh -E 'test(/d3d9/)'
#
# Env knobs (all optional, all ${VAR:-default} overridable):
#   WIE_FAST_PROFILE  nextest profile to use          (default: fast)
#   WIE_FAST_ARGS     extra args, whitespace-split   (default: empty)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PROFILE="${WIE_FAST_PROFILE:-fast}"

# The persistent on-disk JIT ledger is already disabled inside test processes
# (crates/wie-cpu/src/jit/cache_persist.rs keys off `cfg(test)` / `NEXTEST`).
# Exporting it here as well is belt-and-braces for the plain-`cargo test` case,
# where no `NEXTEST` marker exists, and costs nothing when nextest is used.
export WIE_JIT_CACHE="${WIE_JIT_CACHE:-0}"

# The filter the `fast` profile excludes. Kept here only for the "how do I run
# what I skipped" hint below; the profile itself owns the selection.
SLOW_FILTER='package(wie-runtime) + binary(/^(micro_|clock_stub$|idle_park_wake$)/)'

# shellcheck disable=SC2206 # deliberate word-splitting of the knob
EXTRA=(${WIE_FAST_ARGS:-} "$@")

echo "=== fast lane (nextest profile '$PROFILE') ==="
echo "--- skipped: wie-runtime micro_gui_window   (13 tests, serialized by the"
echo "---          process-wide GUI_SUITE_LOCK; gui_blit is ~11 s of JIT alone)"
echo "--- skipped: wie-runtime micro_* / clock_stub / idle_park_wake"
echo "---          (real guest PEs driven through the JIT)"
echo "--- run just those: cargo nextest run --profile default \\"
echo "---                 -E '$SLOW_FILTER'"
echo

set +e
# No `--workspace` flag: nextest already defaults to every workspace member,
# and passing it would defeat a caller-supplied `-p` (cargo unions the two and
# builds the whole graph anyway).
cargo nextest run \
  --manifest-path "$ROOT/Cargo.toml" \
  --profile "$PROFILE" \
  "${EXTRA[@]}"
rc=$?
set -e

if (( rc != 0 )); then
  echo
  echo "fast lane FAILED (rc=$rc) — fix, then run scripts/check.sh"
  exit "$rc"
fi

echo
echo "fast lane green. It skipped the two slow groups above; the"
echo "authoritative pre-PR gate is scripts/check.sh (full suite + micro-suite)."
