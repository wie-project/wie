#!/usr/bin/env bash
# Capture WIE_RUNTIME_PROFILE baselines for Phase 0 + Wave 3.
# Scope: docs/perf-plan.md Phase 0 (measurement harness) + docs/perf-plan-2.md Wave 3
#        (park-vs-yield paired captures in docs/baselines/).
#
# Usage:
#   ./scripts/capture-baseline.sh [exe_path] [duration_secs=40]
#   ./scripts/capture-baseline.sh <exe-path> <label> [seconds=40]   # legacy single-label mode
#
# Primary (Phase 0/Wave 3) mode — 0-2 args where the optional second arg is numeric:
#   Builds release (cargo build -p wie-cli --release), then runs the guest twice:
#     WIE_RUNTIME_PROFILE=1 WIE_IDLE=park  for <duration> seconds
#     WIE_RUNTIME_PROFILE=1 WIE_IDLE=yield for <duration> seconds
#   Uses a SIGINT watchdog (the pump prints on SIGINT via profile_sigint_armed)
#   and falls back to `timeout --signal=INT` when available. Captures stdout+stderr
#   to docs/baselines/park.txt (+ .json copy) and docs/baselines/yield.txt (+ .json).
#   Verifies the SIGINT→profile handoff (grep idle_residency_ms / emu_ms) and then
#   prints a wall/CPU%, stall, iced-share and idle-attribution diff via python3.
#
# Legacy mode — <exe> <label> [seconds] where <label> is non-numeric:
#   Preserves the original single-capture contract (scripts/capture-baseline.sh
#   <exe> <label> [seconds]) — still builds release, still uses the SIGINT watchdog,
#   still verifies the profile, but writes only docs/baselines/<label>.txt (+ .json).
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
#      finished before the watchdog fires); 124 from `timeout` is also accepted.
#   6. The report must contain idle_residency_ms / emu_ms / wall_ms; otherwise
#      the capture is flagged (grep check). If flaky or silent, fix Phase 0 first.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEFAULT_EXE="$ROOT/micro-exes/out/long_loop.exe"
OUTDIR="$ROOT/docs/baselines"

usage() {
	echo "usage: $0 [exe_path] [duration_secs=40]" >&2
	echo "   or: $0 <exe-path> <label> [seconds=40]  (legacy single-label mode)" >&2
	exit 2
}

is_int() { [[ "$1" =~ ^[0-9]+$ ]]; }

# ---- arg parsing (new vs legacy) ----
EXE=""
DURATION=40
LABEL=""
LEGACY=0

if [[ $# -eq 0 ]]; then
	EXE="$DEFAULT_EXE"
elif [[ $# -eq 1 ]]; then
	if [[ "$1" == "-h" || "$1" == "--help" ]]; then usage; fi
	if is_int "$1"; then
		EXE="$DEFAULT_EXE"
		DURATION="$1"
	else
		EXE="$1"
	fi
elif [[ $# -eq 2 ]]; then
	if is_int "$2"; then
		EXE="$1"
		DURATION="$2"
	else
		# legacy: exe + label
		EXE="$1"
		LABEL="$2"
		LEGACY=1
	fi
elif [[ $# -eq 3 ]]; then
	# legacy: exe label seconds
	if is_int "$3" && ! is_int "$2"; then
		EXE="$1"
		LABEL="$2"
		DURATION="$3"
		LEGACY=1
	else
		usage
	fi
else
	usage
fi

if ! is_int "$DURATION"; then usage; fi

# ---- resolve exe (default + micro-exes build fallback) ----
resolve_exe() {
	local p="$1"
	if [[ -f "$p" && -x "$p" ]]; then
		echo "$p"
		return 0
	fi
	# candidate missing — try to build micro-exes
	if [[ "$p" == "$DEFAULT_EXE" ]]; then
		echo "note: $p not found — trying make -C micro-exes" >&2
		if command -v make >/dev/null 2>&1; then
			if make -C "$ROOT/micro-exes" long_loop >/dev/null 2>&1 || make -C "$ROOT/micro-exes" >/dev/null 2>&1; then
				if [[ -f "$DEFAULT_EXE" ]]; then
					# ensure executable bit for WSL/macOS
					chmod +x "$DEFAULT_EXE" 2>/dev/null || true
					if [[ -x "$DEFAULT_EXE" ]]; then
						echo "$DEFAULT_EXE"
						return 0
					fi
				fi
			fi
		fi
		echo "warning: default exe $DEFAULT_EXE not found and micro-exes build did not produce it — skipping capture" >&2
		echo "hint: install x86_64-w64-mingw32-gcc and run: make -C micro-exes long_loop" >&2
		return 1
	fi
	echo "error: guest exe not found or not executable: $p" >&2
	return 1
}

if ! EXE="$(resolve_exe "$EXE")"; then
	# For the default-exe case we exit 0 (skip with warning) to keep CI idempotent;
	# for an explicit exe we fail.
	if [[ -z "${EXE:-}" && -n "$DEFAULT_EXE" ]]; then
		# resolve_exe already warned; check if caller asked for default
		if [[ $# -eq 0 ]]; then
			echo "capture skipped (no exe to run)" >&2
			mkdir -p "$OUTDIR"
			[[ -f "$OUTDIR/.gitkeep" ]] || : > "$OUTDIR/.gitkeep"
			exit 0
		fi
		if [[ $# -eq 1 ]] && is_int "$1"; then
			echo "capture skipped (no exe to run)" >&2
			mkdir -p "$OUTDIR"
			[[ -f "$OUTDIR/.gitkeep" ]] || : > "$OUTDIR/.gitkeep"
			exit 0
		fi
	fi
	exit 1
fi

# ---- build release (always) ----
BIN="$ROOT/target/release/wie"
echo "=== building release: cargo build -p wie-cli --release ===" >&2
cargo build -p wie-cli --release --manifest-path "$ROOT/Cargo.toml"

if [[ ! -x "$BIN" ]]; then
	echo "error: $BIN missing after build" >&2
	exit 1
fi

mkdir -p "$OUTDIR"
# ensure baselines dir is tracked even when captures are skipped
if [[ ! -f "$OUTDIR/.gitkeep" && ! -f "$OUTDIR/README.md" ]]; then
	: > "$OUTDIR/.gitkeep"
fi

# ---- capture helper (SIGINT watchdog, timeout-aware) ----
capture_one() {
	local policy="$1"  # park | yield | custom label policy not used in legacy
	local out="$2"
	local secs="$3"
	local exe="$4"

	echo "=== capture: WIE_IDLE=$policy WIE_RUNTIME_PROFILE=1 $BIN run $exe (${secs}s) -> $out ===" >&2
	mkdir -p "$(dirname "$out")"
	# remove stale out so grep check is not fooled by old file
	rm -f "$out"

	local status=0
	local use_timeout=0
	if command -v timeout >/dev/null 2>&1 && timeout --help 2>&1 | grep -q -- "--signal"; then
		use_timeout=1
	fi

	if [[ $use_timeout -eq 1 ]]; then
		# Prefer timeout --signal=INT: it delivers exactly what profile_sigint_armed expects.
		# Kill-after ensures we don't hang if the guest ignores SIGINT.
		set +e
		WIE_RUNTIME_PROFILE=1 WIE_IDLE="$policy" timeout --signal=INT --kill-after=5s "${secs}s" "$BIN" run "$exe" >"$out" 2>&1
		status=$?
		set -e
		# timeout exit codes: 124 on timeout, 128+signal on kill, 130 from child SIGINT is also possible.
		# The profile report should still be in $out in all cases.
	else
		# Watchdog: sleep then SIGINT the child (exactly what the emulator's profiling gate expects).
		set +e
		WIE_RUNTIME_PROFILE=1 WIE_IDLE="$policy" "$BIN" run "$exe" >"$out" 2>&1 &
		local pid=$!
		(
			sleep "$secs"
			kill -INT "$pid" 2>/dev/null || true
		) &
		local watchdog=$!
		wait "$pid" || status=$?
		kill "$watchdog" 2>/dev/null || true
		wait "$watchdog" 2>/dev/null || true
		set -e
	fi

	if [[ $status -ne 0 && $status -ne 130 && $status -ne 124 ]]; then
		echo "warning: emulator exited with status $status (expected 0, 124 or 130) — capture kept: $out" >&2
	fi

	# Verify SIGINT→profile handoff: the pump prints the report on HostInterrupt.
	if grep -qE "idle_residency_ms|emu_ms|=== WIE_RUNTIME_PROFILE ===" "$out"; then
		echo "profile report present in $out" >&2
	else
		echo "warning: $out missing profile report (grep idle_residency_ms/emu_ms/=== WIE_RUNTIME_PROFILE === failed)" >&2
		echo "  SIGINT→profile handoff may be flaky — check crates/wie-winapi/src/console/host_term.rs and pump.rs" >&2
		echo "  tail of $out:" >&2
		tail -n 40 "$out" >&2 || true
	fi

	# Ensure .json copy exists (task asks for park.json/yield.json; report is text but we keep both extensions)
	local json_copy="${out%.txt}.json"
	if [[ "$out" != "$json_copy" ]]; then
		cp -f "$out" "$json_copy" 2>/dev/null || true
	fi
	# Also ensure the opposite extension exists when out is .json
	local txt_copy="${out%.json}.txt"
	if [[ "$out" != "$txt_copy" && "$out" == *.json ]]; then
		cp -f "$out" "$txt_copy" 2>/dev/null || true
	fi
}

# ---- legacy single-label mode ----
if [[ $LEGACY -eq 1 ]]; then
	if [[ -z "$LABEL" ]]; then usage; fi
	# sanitize label to a filename (strip slashes)
	LABEL="$(basename "$LABEL")"
	OUT_TXT="$OUTDIR/${LABEL}.txt"
	# Legacy also builds release (done above) and uses park policy unless label hints yield/park
	# Keep original semantics: run with default env (no forced WIE_IDLE) ? No — we keep watchdog
	# but default to park if label not park/yield. For reproducibility, honor WIE_IDLE if caller
	# already exported it; otherwise use park.
	POLICY="park"
	if [[ "$LABEL" == *"yield"* ]]; then POLICY="yield"; fi
	if [[ "$LABEL" == *"park"* ]]; then POLICY="park"; fi
	capture_one "$POLICY" "$OUT_TXT" "$DURATION" "$EXE"
	echo "baseline written: $OUT_TXT (and .json copy)" >&2
	exit 0
fi

# ---- primary mode: paired park + yield ----
PARK_TXT="$OUTDIR/park.txt"
YIELD_TXT="$OUTDIR/yield.txt"
PARK_JSON="$OUTDIR/park.json"
YIELD_JSON="$OUTDIR/yield.json"

capture_one "park" "$PARK_TXT" "$DURATION" "$EXE"
capture_one "yield" "$YIELD_TXT" "$DURATION" "$EXE"

# Ensure json copies (capture_one already copies txt->json, but be explicit for task check)
cp -f "$PARK_TXT" "$PARK_JSON" 2>/dev/null || true
cp -f "$YIELD_TXT" "$YIELD_JSON" 2>/dev/null || true

echo "baselines written:" >&2
ls -lh "$PARK_TXT" "$YIELD_TXT" "$PARK_JSON" "$YIELD_JSON" 2>&1 | sed 's/^/  /' >&2 || true

# ---- compare: wall/CPU%, stall totals, iced share, idle attribution delta ----
echo "=== diff: park vs yield ===" >&2
python3 - "$PARK_TXT" "$YIELD_TXT" <<'PY' || echo "warning: python diff failed (no python3 or parse error)" >&2
import re, sys, pathlib

def parse_file(path):
    text = pathlib.Path(path).read_text(errors="ignore") if pathlib.Path(path).exists() else ""
    # Extract fields via regex; missing -> None
    def f(pat, cast=float, default=None):
        m = re.search(pat, text)
        if not m: return default
        try:
            return cast(m.group(1))
        except: return default
    def fi(pat, default=None):
        return f(pat, cast=lambda x: int(float(x)), default=default)
    # wall_ms, cpu_user_ms, cpu_sys_ms, cpu%≈
    wall = f(r"wall_ms=([0-9.]+)")
    cpu_u = f(r"cpu_user_ms=([0-9.]+)")
    cpu_s = f(r"cpu_sys_ms=([0-9.]+)")
    cpu_pct = f(r"cpu%[≈~]*([0-9.]+)")
    emu = f(r"emu_ms=([0-9.]+)")
    handler = f(r"handler_ms=([0-9.]+)")
    idle_res = f(r"idle_residency_ms=([0-9.]+)")
    idle_park = f(r"idle_park_ms=([0-9.]+)")
    idle_parks = fi(r"idle_parks=([0-9]+)")
    # jit: insns= iced= compiles= ... bg_stall_us, bg_to, bg_enq etc.
    # profile lines: jit: insns=... iced=...  bg_stall_us=... bg_to etc? And jit_profile: bg_to, bg_enq etc.
    # Try multiple patterns.
    bg_stall = fi(r"bg_stall_us=([0-9]+)")
    if bg_stall is None:
        bg_stall = fi(r"bg_stalls[^0-9]*([0-9]+)")  # fallback
    bg_to = fi(r"bg_to=([0-9]+)")
    bg_enq = fi(r"bg_enq=([0-9]+)")
    if bg_enq is None:
        bg_enq = fi(r"bg_enqueues=([0-9]+)")
    compile_us = fi(r"compile_us=([0-9]+)")
    iced = fi(r"iced=([0-9]+)")
    jit_insns = fi(r"jit: insns=([0-9]+)")
    if jit_insns is None:
        # fallback: try insns field
        jit_insns = fi(r"insns=([0-9]+)")
    # noisy/charged/host_stops
    host_stops = fi(r"host_stops=([0-9]+)")
    noisy = fi(r"noisy=([0-9]+)")
    # idle_policy
    idle_policy = None
    m = re.search(r"idle_policy=([a-z]+)", text)
    if m: idle_policy = m.group(1)
    return {
        "wall": wall, "cpu_u": cpu_u, "cpu_s": cpu_s, "cpu_pct": cpu_pct,
        "emu": emu, "handler": handler,
        "idle_res": idle_res, "idle_park": idle_park, "idle_parks": idle_parks,
        "bg_stall": bg_stall, "bg_to": bg_to, "bg_enq": bg_enq,
        "compile_us": compile_us, "iced": iced, "jit_insns": jit_insns,
        "host_stops": host_stops, "noisy": noisy,
        "idle_policy": idle_policy,
        "path": path,
    }

if len(sys.argv) < 3:
    print("compare needs park and yield files")
    sys.exit(0)
park = parse_file(sys.argv[1])
yld = parse_file(sys.argv[2])

def fmt(v, suffix="", fmt_spec=".2f"):
    if v is None: return "n/a"
    try:
        return f"{v:{fmt_spec}}{suffix}"
    except: return f"{v}{suffix}"

def delta(a,b):
    if a is None or b is None: return "n/a"
    d = a - b
    sign = "+" if d>=0 else ""
    return f"{sign}{d:.2f}"

print(f"{'metric':<22} {'park':>14} {'yield':>14} {'delta(park-yield)':>16}")
print("-"*70)
for key,label in [
    ("wall","wall_ms"), ("cpu_pct","cpu%"), ("emu","emu_ms"),
    ("handler","handler_ms"), ("idle_res","idle_residency_ms"),
    ("idle_park","idle_park_ms"), ("idle_parks","idle_parks"),
    ("bg_stall","bg_stall_us"), ("bg_to","bg_to"), ("bg_enq","bg_enq"),
    ("compile_us","compile_us"), ("iced","iced_insns"), ("jit_insns","jit_insns"),
    ("host_stops","host_stops"), ("noisy","noisy"),
]:
    pv = park[key]
    yv = yld[key]
    d = delta(pv, yv) if isinstance(pv,(int,float)) and isinstance(yv,(int,float)) else "n/a"
    print(f"{label:<22} {fmt(pv):>14} {fmt(yv):>14} {d:>16}")

# extra derived: iced share, idle attribution
def iced_share(d):
    j = d["jit_insns"]; i = d["iced"]
    if j is None or i is None or (j+i)==0: return None
    return i*100.0/(j+i)
ps = iced_share(park); ys = iced_share(yld)
if ps is not None or ys is not None:
    print(f"{'iced_share%':<22} {fmt(ps, '%'):>14} {fmt(ys, '%'):>14} {delta(ps, ys) if ps is not None and ys is not None else 'n/a':>16}")

# cpu% derived if missing
for name, d in [("park",park), ("yield",yld)]:
    if d["cpu_pct"] is None and d["wall"] and d["cpu_u"] is not None and d["cpu_s"] is not None:
        try:
            d["cpu_pct"] = (d["cpu_u"]+d["cpu_s"])/(d["wall"]) * 100.0 if d["wall"] else None
        except: pass

print()
print(f"park:  {park['path']}  idle_policy={park['idle_policy']}")
print(f"yield: {yld['path']}  idle_policy={yld['idle_policy']}")
# highlight idle delta
if park["idle_res"] is not None and yld["idle_res"] is not None:
    print(f"idle_residency delta: park {park['idle_res']:.2f} ms vs yield {yld['idle_res']:.2f} ms (park-yield {park['idle_res']-yld['idle_res']:+.2f} ms)")
if park["cpu_pct"] is not None and yld["cpu_pct"] is not None:
    print(f"cpu% delta: park {park['cpu_pct']:.1f}% vs yield {yld['cpu_pct']:.1f}% (park-yield {park['cpu_pct']-yld['cpu_pct']:+.1f}%)")
PY

echo "done." >&2
