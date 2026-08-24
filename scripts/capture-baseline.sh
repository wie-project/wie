#!/usr/bin/env bash
# Capture a WIE_RUNTIME_PROFILE baseline for one guest EXE (perf-plan Phase 0).
# Usage: ./scripts/capture-baseline.sh <exe-path> <label> [seconds=40]
#
# Build the release CLI FIRST — this script never builds:
#   cargo build -p wie-cli --release
#
# How the stop works (verified in source, do not "simplify" to stdout):
#   1. WIE_RUNTIME_PROFILE arms a cached SIGINT gate at startup
#      (crates/wie-winapi/src/console/host_term.rs: PROFILE_SIGINT_GATE,
#      profile_sigint_gate / profile_sigint_armed; the CLI installs the
#      handler once when armed).
#   2. The libc::signal(SIGINT, on_sigint) handler only sets CTRLC_PENDING
#      (async-signal-safe atomic store, host_term.rs).
#   3. The runtime pump drains it once per quantum via
#      take_ctrlc_for_profile_stop() and ends the session as HostInterrupt
#      (crates/wie-runtime/src/session/pump.rs, top of the pump loop).
#   4. On HostInterrupt the CLI prints the === WIE_RUNTIME_PROFILE === report
#      with eprintln! — i.e. on STDERR
#      (crates/wie-cli/src/commands/run.rs) — so this script captures BOTH
#      stdout and stderr into the file.
#   5. Expected exit status after the interrupt is 130 (0 if the guest
#      finished before the watchdog fires); anything else is warned about.
set -euo pipefail

usage() {
	echo "usage: $0 <exe-path> <label> [seconds=40]" >&2
	exit 2
}

[[ $# -ge 2 && $# -le 3 ]] || usage
exe=$1
label=$2
seconds=${3:-40}
[[ -f "$exe" && -x "$exe" ]] || {
	echo "error: guest exe not found or not executable: $exe" >&2
	exit 1
}
[[ "$seconds" =~ ^[0-9]+$ ]] || usage

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
bin="$ROOT/target/release/wie"
[[ -x "$bin" ]] || {
	echo "error: $bin missing — build it first: cargo build -p wie-cli --release" >&2
	exit 1
}

outdir="$ROOT/docs/baselines"
mkdir -p "$outdir"
out="$outdir/$label.txt"

WIE_RUNTIME_PROFILE=1 "$bin" run "$exe" >"$out" 2>&1 &
pid=$!

# Watchdog: deliver SIGINT after <seconds>. kill -INT is exactly what the
# emulator's profiling gate expects (see header); if the guest already
# exited, the signal goes nowhere and the `|| true` keeps us clean.
status=0
(
	sleep "$seconds"
	kill -INT "$pid" 2>/dev/null || true
) &
watchdog=$!
wait "$pid" || status=$?
kill "$watchdog" 2>/dev/null || true
wait "$watchdog" 2>/dev/null || true

if [[ $status -ne 0 && $status -ne 130 ]]; then
	echo "warning: emulator exited with status $status (expected 0 or 130)" >&2
	echo "capture kept anyway: $out" >&2
fi
echo "baseline written: $out"
