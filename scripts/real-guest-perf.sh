#!/usr/bin/env bash
# Real-guest performance harness: calibrated 7-Zip workloads, interleaved A/B.
#
# `docs/architecture/performance-ceiling.md` §8.5 asks for exactly this: its
# numbers came from ad-hoc scripts, and the three mistakes that invalidated
# earlier rounds are designed out here rather than documented around.
#
# ## What it measures
#
# One row per (class, arm, rep). A *class* is one 7-Zip invocation over a fixed
# corpus; an *arm* is the same class under one env difference (the baseline is
# `def`). Everything else — binary, corpus, cache directory, drive mapping — is
# held constant, because a comparison between two independently built trees or
# two corpora measures the trees, not the knob.
#
# ## The three hazards this script refuses to repeat
#
#   1. **The archive must live on `D:`.** Guest `C:\` is the host bottle
#      (`~/Library/Application Support/WIE/bottle/drive_c`), so `rm -f` on a
#      `C:`-relative archive silently does nothing: the next `7za a` *updates*
#      an existing archive instead of compressing from scratch, and the work
#      drops ~4x (measured: 1.92 G guest insns fresh vs 0.507 G on an unchanged
#      archive). Every archive here is written under `--corpus` — a host
#      directory passed as `--drive-d` — and deleted there before each run. The
#      script refuses a corpus that IS the bottle, and refuses to start a run
#      whose archive it could not delete.
#   2. **Host load is not a constant.** Absolute times move 20 %+ between
#      sessions on a shared machine (mx9 measured 32.5-39.7 s within one
#      session). Only *ratios* from interleaved pairs are reported as results;
#      absolute times are printed as a spread and labelled; and a rep that
#      starts above `--max-load` (default 2.0, the same convention and
#      `WIE_PERF_MAX_LOAD` override as `scripts/check.sh --only perf`) is
#      skipped rather than measured. If every rep is skipped this exits
#      non-zero with no numbers rather than reporting the host.
#   3. **A corpus you cannot reproduce is folklore.** `--make-corpus` writes the
#      deterministic set (xorshift random, patterned text, patterned mid) that
#      this script's calibration figures refer to; every run verifies sizes
#      *and* CRCs against that manifest and refuses to measure a drifted corpus.
#
# ## Usage
#
#   ./scripts/real-guest-perf.sh                  # default matrix, 3 reps
#   ./scripts/real-guest-perf.sh --classes mx9 --reps 2
#   ./scripts/real-guest-perf.sh --make-corpus /tmp/wie-perf-corpus
#   ./scripts/real-guest-perf.sh --arms def,chain0,opt_speed
#   ./scripts/real-guest-perf.sh --tsv > runs.tsv # rows only, for awk/pandas
#
# Default classes: mx1s, mx9s, h (about 14 s of guest work per arm per rep).
# Default arms:    def, chain0, opt_speed.
# `mx9` and `mx1` (full 6.7 MB corpus) are opt-in: mx9 is about 1 min per run.
#
# Classes (corpus, then the guest argv after `--`):
#   mx9   full   a -mx9 -y D:\mx9.7z D:\rnd D:\txt D:\mid   LZMA, compute-heavy
#   mx1   full   a -mx1 -y D:\mx1.7z D:\rnd D:\txt D:\mid   LZMA fast, I/O mix
#   mx9s  small  the same at -mx9 on the 1.3 MB subset       knob ablations
#   mx1s  small  the same at -mx1 on the 1.3 MB subset
#   x     full   x -y -oD:\ex D:\src.7z                      extraction
#   h     full   h D:\rnd\r0.bin                             read+hash
#
# Arms (env difference from `def`):
#   def        (none)
#   chain0     WIE_JIT_CHAIN=0       no FuncRef chaining / chain table
#   opt_speed  WIE_JIT_OPT=speed     Cranelift optimiser on every block
#   workers0   WIE_JIT_BG=0          no background compile worker
#   workers1   WIE_JIT_WORKERS=1     one compile worker
#   mem_slow   WIE_JIT_MEM=slow      helper-only memory (oracle, ~28x)
#   mem_pin    WIE_JIT_MEM=pin       top-2 data-pin IR added to the sticky TLB
#   mem_sticky WIE_JIT_MEM=sticky    sticky TLB only, no data-pin stage
#   simd0      WIE_JIT_SIMD=0        scalar XMM lowering
#
# Exit status: 0 measured, 1 a run failed, 2 usage/binary/corpus problem,
# 3 every rep skipped for host load.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CLI="${CLI:-$ROOT/target/release/wie}"
SEVENZA="${SEVENZA:-$ROOT/real_exes/7za.exe}"

CORPUS="${WIE_PERF_CORPUS:-/tmp/wie-perf-corpus}"
OUT="${WIE_PERF_OUT:-/tmp/wie-perf}"
REPS="${WIE_PERF_REPS:-3}"
CLASSES="mx1s,mx9s,h"
ARMS="def,chain0,opt_speed"
MAX_LOAD="${WIE_PERF_MAX_LOAD:-2.0}"
MAX_API="${WIE_PERF_MAX_API:-400000000}"
TIMEOUT="${WIE_PERF_TIMEOUT:-900}"
TSV=0

usage() { sed -n '2,68p' "$0" | sed 's/^# \{0,1\}//'; }

# ---------------------------------------------------------------------------
# Corpus: deterministic generator + canonical manifest
#
# Regenerating rewrites the same bytes (fixed seeds, fixed patterns), so the
# manifest below stays valid; `--make-corpus` verifies what it just wrote.
# ---------------------------------------------------------------------------
CANONICAL_MANIFEST='rnd/r0.bin 2777822253 2097152
rnd/r1.bin 153302691 2097152
txt/t0.txt 2459671235 2097152
mid/m0.bin 1335257007 1048576
small/rnd/r0.bin 340041785 524288
small/rnd/r1.bin 1638535364 524288
small/txt/t0.txt 2494204619 524288
small/mid/m0.bin 1232347931 262144'

CORPUS_FILES="rnd/r0.bin rnd/r1.bin txt/t0.txt mid/m0.bin
small/rnd/r0.bin small/rnd/r1.bin small/txt/t0.txt small/mid/m0.bin"


make_corpus() {
  local dir="$1"
  mkdir -p "$dir"/rnd "$dir"/txt "$dir"/mid \
           "$dir"/small/rnd "$dir"/small/txt "$dir"/small/mid
  python3 - "$dir" <<'PY'
import sys

root = sys.argv[1]

def xorshift(path, n, seed):
    state = seed
    out = bytearray()
    while len(out) < n:
        state ^= (state << 13) & 0xFFFFFFFFFFFFFFFF
        state ^= state >> 7
        state ^= (state << 17) & 0xFFFFFFFFFFFFFFFF
        out += state.to_bytes(8, "little")
    with open(path, "wb") as f:
        f.write(bytes(out[:n]))

WORDS = ("alpha beta gamma delta region superblock lowering guard pin trace "
         "block frame slice verifier guest host cranelift chain").split()

def text(path, n):
    out = []
    total = 0
    i = 0
    while total < n:
        line = "%06d %s\n" % (i, " ".join(WORDS[(i * 7 + j) % len(WORDS)] for j in range(12)))
        out.append(line)
        total += len(line)
        i += 1
    with open(path, "wb") as f:
        f.write("".join(out).encode()[:n])

def mid(path, n):
    state = 0x2545F4914F6CDD1D
    out = bytearray()
    while len(out) < n:
        out += b"wie-mid-block" + (state & 0xFFFFFFFF).to_bytes(4, "little")
        state = (state * 6364136223846793005 + 1442695040888963407) & 0xFFFFFFFFFFFFFFFF
    with open(path, "wb") as f:
        f.write(bytes(out[:n]))

MB = 1024 * 1024
xorshift("%s/rnd/r0.bin" % root, 2 * MB, 0x0139408DCBBF7A44)
xorshift("%s/rnd/r1.bin" % root, 2 * MB, 0x2545F4914F6CDD1D)
text("%s/txt/t0.txt" % root, 2 * MB)
mid("%s/mid/m0.bin" % root, MB)
xorshift("%s/small/rnd/r0.bin" % root, MB // 2, 0x0139408DCBBF7A44)
xorshift("%s/small/rnd/r1.bin" % root, MB // 2, 0x2545F4914F6CDD1D)
text("%s/small/txt/t0.txt" % root, MB // 2)
mid("%s/small/mid/m0.bin" % root, MB // 4)
PY
}

# `cksum` is POSIX (CRC + size), so this works wherever the script does.
manifest_of() {
  local dir="$1" rel
  for rel in $CORPUS_FILES; do
    if [[ -f "$dir/$rel" ]]; then
      printf '%s %s\n' "$rel" "$(cksum < "$dir/$rel" | awk '{print $1" "$2}')"
    else
      printf '%s MISSING\n' "$rel"
    fi
  done
}

verify_corpus() {
  local dir="$1" actual
  actual="$(manifest_of "$dir")"
  [[ "$actual" == "$CANONICAL_MANIFEST" ]] && return 0
  echo "real-guest-perf: corpus at $dir is not the calibrated set." >&2
  echo "  expected (path crc size) | actual (path crc size)" >&2
  paste <(printf '%s\n' "$CANONICAL_MANIFEST") <(printf '%s\n' "$actual") \
    | awk -F'\t' '{ printf "  %-38s | %s\n", $1, $2 }' >&2
  echo "  Regenerate:  $0 --make-corpus $dir" >&2
  echo "  A drifted corpus changes file sizes and compressibility, which moves" >&2
  echo "  both wall time and instruction count. Numbers from it are not" >&2
  echo "  comparable with docs/architecture/performance-ceiling.md." >&2
  return 1
}

# ---------------------------------------------------------------------------
# Host load — the same rule as `scripts/check.sh --only perf`
# ---------------------------------------------------------------------------
host_load() {
  sysctl -n vm.loadavg 2>/dev/null | tr -d '{}' | awk '{print $1}' || true
}

load_above_limit() {
  local load="$1"
  [[ -n "$load" ]] && awk "BEGIN {exit !($load > $MAX_LOAD)}"
}

# ---------------------------------------------------------------------------
# Class → corpus subset + guest argv
# ---------------------------------------------------------------------------
class_corpus() {
  case "$1" in
    mx9s|mx1s) echo "small" ;;
    *)         echo "" ;;
  esac
}

class_argv() {
  local class="$1" level
  case "$class" in
    mx9|mx9s) level=9 ;;
    mx1|mx1s) level=1 ;;
    x|h)      level="" ;;
    *) echo "unknown class: $class" >&2; return 2 ;;
  esac
  case "$class" in
    mx9|mx1|mx9s|mx1s)
      printf 'a\t-mx%s\t-y\tD:\\%s.7z\tD:\\rnd\tD:\\txt\tD:\\mid' "$level" "$class"
      ;;
    x) printf 'x\t-y\t-oD:\\ex\tD:\\src.7z' ;;
    h) printf 'h\tD:\\rnd\\r0.bin' ;;
  esac
}

# Calibrated guest-**instruction** band per class (±10%).
#
# Instruction count is the right calibration check and wall time is not: it is
# independent of host load, and it is exactly the number the archive hazard moves
# (a silently updated archive halves it — 1.92 G fresh vs 0.507 G on an unchanged
# one). Measured on the canonical corpus above, `def` arm, 2026-11-24:
#
#   class  guest insns        (wall on that host, load ~3.4: mx9 75 s, mx1 34 s,
#   mx9    7_623_204_048       mx9s 23 s, mx1s 16 s, x 2 s, h 2.3 s — a load-
#   mx1    2_963_541_685       dependent spread, deliberately not a gate)
#   mx9s   1_839_311_182
#   mx1s     741_002_769
#   x        120_913_700
#   h          5_791_806
#
# Outside the band, the corpus drifted: say so rather than let a reader compare
# the number with docs/architecture/performance-ceiling.md.
class_insn_band() {
  case "$1" in
    mx9)  echo "6860883643 8385524452" ;;
    mx1)  echo "2667187516 3259895853" ;;
    mx9s) echo "1655380063 2023242300" ;;
    mx1s) echo "666902492 815103045" ;;
    x)    echo "108822330 133005070" ;;
    h)    echo "5212625 6370996" ;;
    *)    echo "0 1000000000000" ;;
  esac
}

# ---------------------------------------------------------------------------
# Arm → env spec (space separated KEY=VALUE; empty for the baseline)
# ---------------------------------------------------------------------------
arm_env() {
  case "$1" in
    def)       echo "" ;;
    chain0)    echo "WIE_JIT_CHAIN=0" ;;
    opt_speed) echo "WIE_JIT_OPT=speed" ;;
    workers0)  echo "WIE_JIT_BG=0" ;;
    workers1)  echo "WIE_JIT_WORKERS=1" ;;
    mem_slow)  echo "WIE_JIT_MEM=slow" ;;
    mem_pin)   echo "WIE_JIT_MEM=pin" ;;
    mem_sticky) echo "WIE_JIT_MEM=sticky" ;;
    simd0)     echo "WIE_JIT_SIMD=0" ;;
    *) echo "unknown arm: $1" >&2; return 2 ;;
  esac
}

die() { echo "real-guest-perf: $*" >&2; exit 2; }

# Guest C: is the host bottle. An archive that cannot be deleted is an update in
# disguise (hazard 1 in the header), so refuse any corpus that IS the bottle —
# for --make-corpus too, since that is where the hazard would be created.
BOTTLE="${WIE_ROOT:-$HOME/Library/Application Support/WIE/bottle}"

refuse_bottle_dir() {
  local dir="$1" bottle_real dir_real
  bottle_real="$(cd "$BOTTLE" 2>/dev/null && pwd -P || printf '%s' "$BOTTLE")"
  dir_real="$(cd "$dir" 2>/dev/null && pwd -P || printf '%s' "$dir")"
  case "$dir_real" in
    "$bottle_real"*)
      die "corpus is inside the guest C: bottle ($bottle_real); pass a host directory instead"
      ;;
  esac
}

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------
while [[ $# -gt 0 ]]; do
  case "$1" in
    --make-corpus)
      shift
      dir="${1:-}"
      [[ -n "$dir" ]] || die "--make-corpus needs a directory"
      refuse_bottle_dir "$dir"
      mkdir -p "$dir"
      refuse_bottle_dir "$dir"
      make_corpus "$dir"
      verify_corpus "$dir" || exit 1
      echo "real-guest-perf: wrote and verified the calibrated corpus at $dir"
      exit 0
      ;;
    --reps)     shift; REPS="${1:-}" ;;
    --corpus)   shift; CORPUS="${1:-}" ;;
    --classes)  shift; CLASSES="${1:-}" ;;
    --arms)     shift; ARMS="${1:-}" ;;
    --out)      shift; OUT="${1:-}" ;;
    --max-load) shift; MAX_LOAD="${1:-}" ;;
    --tsv)      TSV=1 ;;
    -h|--help)  usage; exit 0 ;;
    *) die "unknown option: $1 (see --help)" ;;
  esac
  shift
done

for n in "$REPS" "$MAX_LOAD" "$MAX_API" "$TIMEOUT"; do
  [[ "$n" =~ ^[0-9]+([.][0-9]+)?$ ]] || die "not a number: '$n'"
done

[[ -x "$CLI" ]] || die "no release binary at $CLI — cargo build -p wie-cli --release"
[[ -f "$SEVENZA" ]] || die "no guest at $SEVENZA — ./scripts/fetch.sh 7za"
[[ -d "$CORPUS" ]] || die "no corpus at $CORPUS — $0 --make-corpus $CORPUS"

refuse_bottle_dir "$CORPUS"
verify_corpus "$CORPUS" || exit 2

# `timeout` is GNU coreutils (Homebrew on macOS); without it, run uncapped
# rather than failing the sweep.
if command -v timeout >/dev/null 2>&1; then
  TIMEOUT_BIN="$(command -v timeout)"
else
  TIMEOUT_BIN=""
fi

mkdir -p "$OUT" "$OUT/cache"
CACHE="$OUT/cache"
TSV_FILE="$OUT/runs.tsv"
: > "$TSV_FILE"
printf 'run\tclass\tarm\trep\twall_s\tok\tload\tinsns\tentries\tinsns_per_entry\temu_ms\tcompile_us\tbg_stall_us\thost_stops\tslices_max\n' \
  >> "$TSV_FILE"

if [[ "$TSV" == 0 ]]; then
  echo "real-guest-perf: corpus $CORPUS (verified)"
  echo "real-guest-perf: binary $CLI"
  echo "real-guest-perf: classes=[$CLASSES] arms=[$ARMS] reps=$REPS max_load=$MAX_LOAD"
fi

now_s() { python3 -c 'import time;print(time.time())'; }

# Pull one counter out of the `WIE_RUNTIME_PROFILE` block in a run log.
profile_field() {
  awk -v what="$2" '
    { text = text " " $0 }
    END {
      if (what == "insns" && match(text, /(^| )insns=[0-9]+/)) {
        s = substr(text, RSTART, RLENGTH); sub(/.*insns=/, "", s); print s
      } else if (what == "entries" && match(text, /cache_hits=[0-9]+/)) {
        print substr(text, RSTART + 11, RLENGTH - 11)
      } else if (what == "emu_ms" && match(text, /emu_ms=[0-9.]+/)) {
        print substr(text, RSTART + 7, RLENGTH - 7)
      } else if (what == "compile_us" && match(text, /compile_us=[0-9]+/)) {
        print substr(text, RSTART + 11, RLENGTH - 11)
      } else if (what == "bg_stall_us" && match(text, /bg_stall_us=[0-9]+/)) {
        print substr(text, RSTART + 12, RLENGTH - 12)
      } else if (what == "host_stops" && match(text, /host_stops=[0-9]+/)) {
        print substr(text, RSTART + 11, RLENGTH - 11)
      } else if (what == "slices_max" && match(text, /slices_max=[0-9]+/)) {
        print substr(text, RSTART + 11, RLENGTH - 11)
      }
    }' "$1"
}

# The `x` class needs a source archive. Built once per sweep from the same
# corpus with the baseline config, then read by every arm identically.
ensure_source_archive() {
  local dir="$1"
  [[ -f "$dir/src.7z" ]] && return 0
  rm -f "$dir/src.7z"
  # shellcheck disable=SC2086
  env WIE_RUNTIME_PROFILE=1 WIE_CACHE_DIR="$CACHE" \
      ${TIMEOUT_BIN:+"$TIMEOUT_BIN" "$TIMEOUT"} "$CLI" run "$SEVENZA" \
      --max-api "$MAX_API" --drive-d "$dir" -- \
      a -mx1 -y 'D:\src.7z' 'D:\rnd' 'D:\txt' 'D:\mid' \
      > "$OUT/warmup-src.log" 2>&1 || true
  [[ -f "$dir/src.7z" ]] || die "could not build the extraction source archive (see $OUT/warmup-src.log)"
}

# ---------------------------------------------------------------------------
# One run. $1 class, $2 arm, $3 rep, $4 corpus root, $5 env spec.
# Sets RUN_* for the caller.
# ---------------------------------------------------------------------------
run_one() {
  local class="$1" arm="$2" rep="$3" root="$4" spec="$5"
  local log="$OUT/$class-$arm-r$rep.log"
  local dir="$root"
  local -a argv=()
  local load t0 t1 rc per_entry

  [[ "$(class_corpus "$class")" == "small" ]] && dir="$root/small"
  IFS=$'\t' read -r -a argv <<< "$(class_argv "$class")"

  # Fresh archive on the host directory the guest sees as D:. If it survives the
  # delete, `7za a` would update it and the workload would silently shrink.
  case "$class" in
    mx9|mx1|mx9s|mx1s)
      rm -f "$dir/$class.7z"
      if [[ -e "$dir/$class.7z" ]]; then
        die "could not delete $dir/$class.7z — refusing to measure an update"
      fi
      ;;
    x) rm -rf "$dir/ex" ;;
  esac
  [[ "$class" == "x" ]] && ensure_source_archive "$root"

  load="$(host_load)"
  t0="$(now_s)"
  set +e
  # shellcheck disable=SC2086  # the env spec is a deliberate word split
  env WIE_RUNTIME_PROFILE=1 WIE_CACHE_DIR="$CACHE" $spec \
      ${TIMEOUT_BIN:+"$TIMEOUT_BIN" "$TIMEOUT"} "$CLI" run "$SEVENZA" \
      --max-api "$MAX_API" --drive-d "$dir" -- \
      "${argv[@]}" > "$log" 2>&1
  rc=$?
  set -e
  t1="$(now_s)"

  RUN_WALL="$(awk -v a="$t0" -v b="$t1" 'BEGIN {printf "%.3f", b - a}')"
  RUN_LOAD="${load:-?}"
  RUN_OK=0
  if [[ "$rc" == 0 ]] && ! grep -q '^Error:' "$log" && grep -q 'Everything is Ok' "$log"; then
    RUN_OK=1
  fi

  RUN_INSNS="$(profile_field "$log" insns)"
  RUN_ENTRIES="$(profile_field "$log" entries)"
  RUN_EMU_MS="$(profile_field "$log" emu_ms)"
  RUN_COMPILE_US="$(profile_field "$log" compile_us)"
  RUN_BG_STALL_US="$(profile_field "$log" bg_stall_us)"
  RUN_STOPS="$(profile_field "$log" host_stops)"
  RUN_SLICES_MAX="$(profile_field "$log" slices_max)"

  per_entry="?"
  if [[ -n "$RUN_INSNS" && -n "$RUN_ENTRIES" && "$RUN_ENTRIES" != 0 ]]; then
    per_entry="$(awk -v i="$RUN_INSNS" -v e="$RUN_ENTRIES" 'BEGIN {printf "%.1f", i / e}')"
  fi

  printf 'run\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$class" "$arm" "$rep" "$RUN_WALL" "$RUN_OK" "$RUN_LOAD" \
    "${RUN_INSNS:-?}" "${RUN_ENTRIES:-?}" "$per_entry" "${RUN_EMU_MS:-?}" \
    "${RUN_COMPILE_US:-?}" "${RUN_BG_STALL_US:-?}" "${RUN_STOPS:-?}" \
    "${RUN_SLICES_MAX:-?}" >> "$TSV_FILE"
  [[ "$TSV" == 1 ]] && tail -1 "$TSV_FILE"
  return 0
}

# ---------------------------------------------------------------------------
# Sweep: reps are the OUTER loop and the whole matrix is re-walked each rep, so
# a slow drift in host load hits every arm instead of whichever arm happened to
# run while a peer built a release binary. That interleaving is the only reason
# a median of per-pair ratios means anything on a shared host.
# ---------------------------------------------------------------------------
IFS=',' read -r -a CLASS_LIST <<< "$CLASSES"
IFS=',' read -r -a ARM_LIST <<< "$ARMS"

SKIPPED_REPS=0
RAN_REPS=0
LOADS=""
FAILED=0

for ((rep = 1; rep <= REPS; rep++)); do
  load="$(host_load)"
  if load_above_limit "$load"; then
    echo "real-guest-perf: rep $rep SKIPPED — host load ${load} > ${MAX_LOAD}; a loaded host reports itself, not the JIT (override with --max-load)" >&2
    SKIPPED_REPS=$((SKIPPED_REPS + 1))
    continue
  fi
  RAN_REPS=$((RAN_REPS + 1))
  [[ -n "$load" ]] && LOADS="$LOADS $load"

  # Warm the private code-cache ledger once per (class, arm), so no arm pays a
  # cold-compile storm the baseline did not pay. The persistent ledger is keyed
  # on the emit fingerprint, which includes the lowering-shape knobs, so each
  # arm has its own — warming only `def` would charge every arm for compiling
  # what the baseline already had cached (measured on `h`: 20.5 s cold vs 0.4 s
  # warm, i.e. an arm/baseline ratio of 0.17 that is entirely compile cache).
  for class in "${CLASS_LIST[@]}"; do
    for arm in "${ARM_LIST[@]}"; do
      spec="$(arm_env "$arm")" || exit 2
      run_one "$class" "$arm" warm "$CORPUS" "$spec"
      if [[ "$RUN_OK" != 1 ]]; then
        echo "real-guest-perf: warmup $class/$arm did not complete (see $OUT/$class-$arm-rwarm.log)" >&2
        FAILED=1
      fi
    done
  done

  for class in "${CLASS_LIST[@]}"; do
    for arm in "${ARM_LIST[@]}"; do
      spec="$(arm_env "$arm")" || exit 2
      run_one "$class" "$arm" "$rep" "$CORPUS" "$spec"
      if [[ "$RUN_OK" != 1 ]]; then
        echo "real-guest-perf: $class/$arm rep $rep FAILED — see $OUT/$class-$arm-r$rep.log" >&2
        FAILED=1
      fi
    done
  done
done

if [[ "$FAILED" == 1 ]]; then
  echo "real-guest-perf: at least one run did not complete; no summary (rows in $TSV_FILE)." >&2
  exit 1
fi
if [[ "$RAN_REPS" == 0 ]]; then
  echo "real-guest-perf: every rep was skipped for host load; no numbers to report." >&2
  exit 3
fi

median() {
  sort -n | awk '{
      v[NR] = $1
    } END {
      if (NR == 0) print "?"
      else if (NR % 2) print v[(NR + 1) / 2]
      else printf "%.3f", (v[NR / 2] + v[NR / 2 + 1]) / 2
    }'
}

if [[ "$TSV" == 0 ]]; then
  if [[ -n "$LOADS" ]]; then
    loads_sorted="$(printf '%s\n' $LOADS | sort -n)"
    echo "real-guest-perf: host load at rep start — min $(printf '%s\n' "$loads_sorted" | head -1), max $(printf '%s\n' "$loads_sorted" | tail -1), limit ${MAX_LOAD}; reps ran ${RAN_REPS}, skipped ${SKIPPED_REPS}"
  fi
  echo
  echo "=== ratios vs def: median of $RAN_REPS interleaved pair(s); >1 = the arm is slower ==="
  printf '%-6s %-10s %3s %9s %8s %14s %12s %9s %10s\n' \
    class arm n wall_s ratio lo..hi insns/entry entries compile_us
  for class in "${CLASS_LIST[@]}"; do
    for arm in "${ARM_LIST[@]}"; do
      ratios=""
      walls=""
      for ((rep = 1; rep <= REPS; rep++)); do
        base="$(awk -F'\t' -v c="$class" -v r="$rep" \
          '$2 == c && $3 == "def" && $4 == r { print $5; exit }' "$TSV_FILE")"
        armw="$(awk -F'\t' -v c="$class" -v a="$arm" -v r="$rep" \
          '$2 == c && $3 == a && $4 == r { print $5; exit }' "$TSV_FILE")"
        [[ -z "$base" || -z "$armw" ]] && continue
        ratios="$ratios $(awk -v b="$base" -v w="$armw" 'BEGIN {printf "%.3f", w / b}')"
        walls="$walls $armw"
      done
      [[ -z "${ratios// /}" ]] && continue
      n="$(printf '%s\n' $ratios | wc -l | awk '{print $1}')"
      med="$(printf '%s\n' $ratios | median)"
      lo="$(printf '%s\n' $ratios | sort -n | head -1)"
      hi="$(printf '%s\n' $ratios | sort -n | tail -1)"
      wall_med="$(printf '%s\n' $walls | median)"
      counters="$(awk -F'\t' -v c="$class" -v a="$arm" '
        $2 == c && $3 == a && $4 != "warm" { pe = $10; en = $9; cu = $12 }
        END { printf "%s %s %s", pe, en, cu }' "$TSV_FILE")"
      printf '%-6s %-10s %3s %9s %8s %14s %12s %9s %10s\n' \
        "$class" "$arm" "$n" "$wall_med" "$med" "$lo..$hi" \
        "$(printf '%s' "$counters" | awk '{print $1}')" \
        "$(printf '%s' "$counters" | awk '{print $2}')" \
        "$(printf '%s' "$counters" | awk '{print $3}')"
    done
  done

  echo
  echo "=== corpus calibration: guest instructions per class, and the wall spread ==="
  for class in "${CLASS_LIST[@]}"; do
    band="$(class_insn_band "$class")"
    lo_b="$(printf '%s' "$band" | awk '{print $1}')"
    hi_b="$(printf '%s' "$band" | awk '{print $2}')"
    walls="$(awk -F'\t' -v c="$class" '$2 == c && $4 != "warm" && $5 != "" { print $5 }' "$TSV_FILE" | sort -n)"
    calib="$(awk -F'\t' -v c="$class" '$2 == c && $3 == "def" && $4 != "warm" && $8 != "?" { print $8 }' "$TSV_FILE")"
    [[ -z "$walls" || -z "$calib" ]] && continue
    wmin="$(printf '%s\n' "$walls" | head -1)"
    wmax="$(printf '%s\n' "$walls" | tail -1)"
    insns="$(printf '%s\n' "$calib" | sort -n | median)"
    printf '  %-6s %14s guest insns (calibrated %s..%s)   wall %ss..%ss\n' \
      "$class" "$insns" "$lo_b" "$hi_b" "$wmin" "$wmax"
    if awk "BEGIN {exit !($insns < $lo_b || $insns > $hi_b)}"; then
      echo "        ^ instruction count is outside the calibrated band: the corpus drifted" >&2
      echo "          (or the archive was updated rather than created). Regenerate it:" >&2
      echo "            $0 --make-corpus $CORPUS" >&2
      echo "          Ratios within this run stay internally consistent; the absolute work" >&2
      echo "          does not, so do not compare it with the ceiling doc." >&2
    fi
  done
  echo
  echo "note: a class whose wall is only a couple of seconds (\`h\`) is dominated by"
  echo "      compile and first-touch effects; use --reps 5+ before believing a ratio"
  echo "      below ~1.2 there. The compute classes (mx9/mx9s) resolve at reps 2-3."
  echo "rows: $TSV_FILE   logs: $OUT/   (--tsv prints rows only)"
fi
