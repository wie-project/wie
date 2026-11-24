#!/usr/bin/env bash
# Real-application multithreading scaling study for WIE.
#
# Measures 7-Zip Extra's own thread scaling (`-mmt=N`) under the emulator and,
# interleaved with it, the *native* 7-Zip on the same corpus. The native arm is
# the control: it is the ceiling this host can actually deliver in the same
# window, so "the emulator loses X" is separated from "the host was busy".
#
# Why 7-Zip and not a synthetic benchmark: `7za a` is a load-heavy Windows app
# that does its own scheduling, allocation and file IO, exposes its thread count
# on the command line, and runs for tens of seconds — the three properties a
# scaling curve needs. Anything smaller measures the emulator's startup path.
#
# Usage:
#   scripts/mt-scaling.sh --corpus DIR [options]
#   scripts/mt-scaling.sh --make-corpus DIR [--corpus-kib N]
#
# Options:
#   --corpus DIR        corpus directory (guest `D:\corpus`)        [required]
#   --levels LIST       compression levels, comma separated         [1,9]
#   --threads LIST      -mmt values, comma separated               [1,2,4,8]
#   --reps N            interleaved repetitions per cell                  [5]
#   --out DIR           results directory (TSV per run in one file)  [/tmp/wie-mt]
#   --cache DIR         private WIE_CACHE_DIR for the sweep  [<out>/cache]
#   --arms LIST         wie,native (native is the control)      [wie,native]
#   --warm              warm the JIT ledger once per level first  [on]
#   --no-warm           measure a cold ledger (ledger wiped per run)
#   --env K=V           extra host env for the `wie` arm (repeatable)
#   --label TEXT        tag written into the TSV (an A/B label)
#   --timeout S         per-run hard timeout, seconds                   [900]
#   --make-corpus DIR   write a mixed-compressibility corpus to DIR and exit
#   -h, --help          this text
#
# Output is one TSV row per guest run on stdout (header first), so it can be
# piped into a spreadsheet or awk. Diagnostics go to stderr.
#
# Two properties this script deliberately keeps:
#
#   * Interleaving. Repetitions are the OUTER loop and the whole matrix is
#     re-walked, so a slow drift in host load hits every cell instead of
#     whichever cell happened to run while a peer built a release binary. The
#     host is shared; medians of interleaved cells are the only defensible
#     statistic.
#   * A private cache root. The JIT keeps a persistent block ledger keyed on
#     the PE hash, and a peer's runs populate the same default directory. The
#     sweep therefore points WIE_CACHE_DIR at $OUT/cache and warms it once per
#     level, so every cell sees the same ledger instead of racing a peer for it.
#
# `WIE_NO_HOOK_SLICES` is raised because compressing a large file legitimately
# runs hundreds of millions of guest instructions with no Win32 call: at the
# default of 40 slices the run is killed with "emulation stopped without hitting
# fake API hook", which is a measurement artefact, not a speed (see
# docs/architecture/performance-ceiling.md, "Threading").
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CLI="${CLI:-$ROOT/target/release/wie}"
SEVENZA="${SEVENZA:-$ROOT/real_exes/7za.exe}"
NATIVE="${NATIVE:-$(command -v 7zz || command -v 7z || true)}"
# `timeout` is GNU coreutils (Homebrew on macOS); fall back to no cap rather
# than failing the sweep on a host that does not have it.
if command -v timeout >/dev/null; then TIMEOUT_BIN="$(command -v timeout)"; else TIMEOUT_BIN=""; fi

CORPUS=""
LEVELS="1,9"
THREADS="1,2,4,8"
REPS=5
OUT="/tmp/wie-mt"
CACHE=""
ARMS="wie,native"
WARM=1
LABEL=""
TIMEOUT=900
MAKE_CORPUS=""
CORPUS_KIB=5220

usage() { sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'; }

EXTRA_ENV=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --corpus) CORPUS="$2"; shift 2 ;;
    --levels) LEVELS="$2"; shift 2 ;;
    --threads) THREADS="$2"; shift 2 ;;
    --reps) REPS="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    --cache) CACHE="$2"; shift 2 ;;
    --arms) ARMS="$2"; shift 2 ;;
    --warm) WARM=1; shift ;;
    --no-warm) WARM=0; shift ;;
    --env) EXTRA_ENV+=("$2"); shift 2 ;;
    --label) LABEL="$2"; shift 2 ;;
    --timeout) TIMEOUT="$2"; shift 2 ;;
    --make-corpus) MAKE_CORPUS="$2"; shift 2 ;;
    --corpus-kib) CORPUS_KIB="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "mt-scaling: unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

# ---------------------------------------------------------------------------
# Corpus: mixed compressibility, so the workload is not one degenerate case.
# A pure-text corpus measures only the match finder's fast path; a pure-random
# corpus measures only the literal coder. Real archives are both, and the two
# have different sharing behaviour (random blocks barely touch the hash chain;
# text blocks hammer it), which is exactly what a contended heap or a contended
# allocator would show up in.
# ---------------------------------------------------------------------------
make_corpus() {
  local dst="$1" kib="$2"
  python3 - "$dst" "$kib" <<'PY'
import os, random, sys
dst, total_kib = sys.argv[1], int(sys.argv[2])
random.seed(20261124)
os.makedirs(dst, exist_ok=True)
for name in os.listdir(dst):
    os.remove(os.path.join(dst, name))
words = [b"alpha", b"beta", b"gamma", b"delta", b"epsilon", b"zeta", b"eta",
         b"theta", b"iota", b"kappa", b"lambda", b"mu", b"nu", b"xi",
         b"omicron", b"pi"]
def write_text(path, nbytes):
    with open(path, "wb") as f:
        w = 0
        while w < nbytes:
            line = b" ".join(random.choice(words) for _ in range(12)) + b"\n"
            f.write(line)
            w += len(line)
# 48% text, 12% random, 24% structured, 16% interleaved mixed.
sixths = [total_kib * p // 100 for p in (8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 4)]
for i in range(6):
    write_text(os.path.join(dst, f"text_{i}.txt"), sixths[i] * 1024)
for i in range(3):
    with open(os.path.join(dst, f"rand_{i}.bin"), "wb") as f:
        f.write(os.urandom(sixths[6 + i] * 1024))
for i in range(2):
    with open(os.path.join(dst, f"struct_{i}.bin"), "wb") as f:
        f.write(bytes(range(256)) * (sixths[9 + i] * 4))
for i in range(2):
    with open(os.path.join(dst, f"mixed_{i}.bin"), "wb") as f:
        for _ in range(6):
            f.write(os.urandom(sixths[11] * 1024 // 6))
            f.write(b"".join(random.choice(words) for _ in range(8000)))
PY
  echo "mt-scaling: corpus at $dst ($(du -sk "$dst" | cut -f1) KiB, \
$(ls -1 "$dst" | wc -l | tr -d ' ') files)" >&2
}

if [[ -n "$MAKE_CORPUS" ]]; then
  make_corpus "$MAKE_CORPUS" "$CORPUS_KIB"
  exit 0
fi

[[ -n "$CORPUS" ]] || { echo "mt-scaling: --corpus is required" >&2; exit 2; }
[[ -d "$CORPUS" ]] || { echo "mt-scaling: no such corpus dir: $CORPUS" >&2; exit 2; }
[[ -x "$CLI" ]] || { echo "mt-scaling: build first: cargo build -p wie-cli --release" >&2; exit 2; }
[[ -f "$SEVENZA" ]] || { echo "mt-scaling: 7za.exe missing: $SEVENZA (./scripts/fetch.sh 7za)" >&2; exit 2; }

mkdir -p "$OUT"
CACHE="${CACHE:-$OUT/cache}"
mkdir -p "$CACHE"

# The bottle root is the corpus PARENT: guest D:\corpus is the corpus dir, and
# the 7z archive is written next to it as D:\out.7z.
DRIVE_D="$(cd "$CORPUS/.." && pwd)"
CORPUS_NAME="$(basename "$CORPUS")"

has_arm() { [[ ",$ARMS," == *",$1,"* ]]; }

# ---------------------------------------------------------------------------
# One emulator run. Everything except the three numbers we keep is discarded.
# ---------------------------------------------------------------------------
run_wie() {
  local level="$1" mt="$2" rep="$3"
  local log="$OUT/run-wie-mx${level}-mmt${mt}-r${rep}.log"
  local t0 t1 wall_s
  rm -f "$DRIVE_D/out.7z"
  local load
  load="$(uptime | sed 's/.*load averages: //; s/ .*//')"
  t0=$(python3 -c 'import time;print(time.time())')
  # ${EXTRA_ENV[@]+...} guards the empty-array expansion: macOS ships bash 3.2,
  # where `"${arr[@]}"` under `set -u` is an unbound-variable error.
  env WIE_RUNTIME_PROFILE=1 \
      WIE_CACHE_DIR="$CACHE" \
      WIE_NO_HOOK_SLICES=1000000 \
      ${EXTRA_ENV[@]+"${EXTRA_ENV[@]}"} \
      ${TIMEOUT_BIN:+"$TIMEOUT_BIN" "$TIMEOUT"} "$CLI" run "$SEVENZA" \
        --drive-d "$DRIVE_D" --max-api 400000000 \
        -- a "-mx$level" "-mmt=$mt" 'D:\out.7z' "D:\\$CORPUS_NAME\\*" \
        >"$log" 2>&1 || true
  t1=$(python3 -c 'import time;print(time.time())')
  wall_s=$(python3 -c "print(f'{$t1-$t0:.3f}')")
  local rc=0
  grep -q "Everything is Ok" "$log" || rc=1
  emit_row "wie" "$LABEL" "$level" "$mt" "$rep" "$wall_s" "$load" "$rc" "$log"
}

# ---------------------------------------------------------------------------
# One native run: the control arm.
# ---------------------------------------------------------------------------
run_native() {
  local level="$1" mt="$2" rep="$3"
  local log="$OUT/run-native-mx${level}-mmt${mt}-r${rep}.log"
  local t0 t1 wall_s load
  rm -f "$OUT/native.7z"
  load="$(uptime | sed 's/.*load averages: //; s/ .*//')"
  t0=$(python3 -c 'import time;print(time.time())')
  ${TIMEOUT_BIN:+"$TIMEOUT_BIN" "$TIMEOUT"} "$NATIVE" a "-mx$level" "-mmt=$mt" \
      "$OUT/native.7z" "$CORPUS"/* >"$log" 2>&1 || true
  t1=$(python3 -c 'import time;print(time.time())')
  wall_s=$(python3 -c "print(f'{$t1-$t0:.3f}')")
  local rc=0
  grep -q "Everything is Ok" "$log" || rc=1
  emit_row "native" "$LABEL" "$level" "$mt" "$rep" "$wall_s" "$load" "$rc" "$log"
}

# ---------------------------------------------------------------------------
# Pull the profile fields out of a run log. `emu_ms` is the *primary thread's*
# accounted emulation time: on a multithreaded guest the workers' emu time is
# not in it (they only publish JitStats), so it is reported for completeness and
# never used as the scaling statistic — wall clock and process cpu% are.
# ---------------------------------------------------------------------------
wall_ms_of() { sed -n 's/^wall_ms=\([0-9.]*\).*/\1/p' "$1" | head -1; }
emu_ms_of() { sed -n 's/^emu_ms=\([0-9.]*\).*/\1/p' "$1" | head -1; }
cpu_of() { sed -n 's/.*cpu%≈\([0-9.]*\).*/\1/p' "$1" | head -1; }
stops_of() { sed -n 's/^host_stops=\([0-9]*\).*/\1/p' "$1" | head -1; }

HEADER="arm	label	level	mmt	rep	wall_s	wall_ms	emu_ms	cpu_pct	host_stops	load1	rc"
emit_row() {
  local arm="$1" label="$2" level="$3" mt="$4" rep="$5" wall_s="$6" load="$7" rc="$8" log="$9"
  if [[ "$arm" == "wie" ]]; then
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
      "$arm" "$label" "$level" "$mt" "$rep" "$wall_s" \
      "$(wall_ms_of "$log")" "$(emu_ms_of "$log")" "$(cpu_of "$log")" \
      "$(stops_of "$log")" "$load" "$rc"
  else
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
      "$arm" "$label" "$level" "$mt" "$rep" "$wall_s" "" "" "" "" "$load" "$rc"
  fi
}

printf '%s\n' "$HEADER"

IFS=',' read -r -a level_list <<<"$LEVELS"
IFS=',' read -r -a thread_list <<<"$THREADS"

# Warm-up: one run per level at the largest thread count, so the ledger is warm
# for every cell. Rows carry rep=0 and are excluded by the analysis.
if [[ "$WARM" == 1 ]] && has_arm wie; then
  for level in "${level_list[@]}"; do
    echo "mt-scaling: warming ledger for -mx$level" >&2
    run_wie "$level" "${thread_list[$((${#thread_list[@]} - 1))]}" 0 >/dev/null
  done
fi

# Interleaved: repetitions outermost, the whole matrix re-walked each time.
for rep in $(seq 1 "$REPS"); do
  for level in "${level_list[@]}"; do
    for mt in "${thread_list[@]}"; do
      if has_arm wie; then run_wie "$level" "$mt" "$rep"; fi
      if has_arm native && [[ -n "$NATIVE" ]]; then run_native "$level" "$mt" "$rep"; fi
      echo "mt-scaling: rep $rep/$REPS mx$level mmt=$mt done" >&2
    done
  done
done
