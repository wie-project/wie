#!/usr/bin/env bash
# Wave 4 instruction-coverage harness.
#
# Runs every built micro-exe under the profiler and both opcode histograms,
# keeps each exe's profile, and appends one aggregated coverage +
# opcode-frequency baseline to docs/baselines/insn-coverage.txt.
#
# Usage: ./scripts/coverage-report.sh [exe-name ...]
#   WIE_CPU=jit|iced   backend to measure (default: jit)
#
# What it writes:
#   micro-exes/out/<name>.profile.txt       per-exe stderr report (kept)
#   micro-exes/out/<name>.profile.stdout   per-exe stdout (tracing dumps, kept)
#   docs/baselines/insn-coverage.txt         append-only aggregate baseline
#
# Notes:
# - Nothing here re-implements a histogram: the opcode-frequency section is
#   extracted verbatim from the `--- jit iced-residue opcode histogram ... ---`
#   block that `render_mnemonic_histogram` already prints, so the baseline can
#   never drift from the runtime's own accounting.
# - `WIE_EXEC_TRACE=1` also prints the *unsampled* interpreter histogram
#   (total = every interpreted step), which is the honest denominator for
#   residue; the JIT one is sampled 1/64 and is used for shape, not volume.
# - The `total_insns` / `jit%` columns are DYNAMIC retired-instruction counts
#   (`basis=dynamic_retired`): the lowering step emits a per-block trip counter
#   so a self-looping block is charged once per trip, not once per entry, and
#   guest worker-thread engines are merged into the profile. The one known
#   undercount is a REP string helper charged 1 instruction instead of `rcx`
#   iterations, so treat a `rep movs*`-dominated row as a lower bound.
#   `insn/entry` is the smell detector, and it now reads the other way from
#   before: HIGH = a hot self-loop retiring a lot per entry, LOW =
#   call/edge-bound code. The opcode histograms below remain exact (they count
#   interpreted steps) and are still how ISA families are ranked; cross-check a
#   surprising row against `WIE_EXEC_TRACE=1` rather than reasoning about blocks.
# - Non-zero guest exits abort the run. The single documented exception is
#   long_loop.exe under `WIE_CPU=iced`, whose 100M-iteration loop blows the
#   default `instruction_budget`; it is skipped, not tolerated silently.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/micro-exes/out"
CLI="${CLI:-$ROOT/target/release/wie}"
CPU="${WIE_CPU:-jit}"
BASELINE="$ROOT/docs/baselines/insn-coverage.txt"

if [[ ! -x "$CLI" ]]; then
  echo "building wie (release)…" >&2
  cargo build -p wie-cli --release --manifest-path "$ROOT/Cargo.toml"
fi

if [[ ! -d "$OUT" ]]; then
  echo "FAIL: $OUT does not exist — run 'make -C micro-exes' first" >&2
  exit 1
fi

export WIE_CPU="$CPU"
export WIE_RUNTIME_PROFILE=1
export WIE_JIT_OPCODE_HISTO=1
export WIE_EXEC_TRACE=1

# ---------------------------------------------------------------- exe list ---
if [[ $# -gt 0 ]]; then
  EXES=()
  for name in "$@"; do
    [[ -f "$OUT/$name" ]] || { echo "FAIL: $OUT/$name not built" >&2; exit 1; }
    EXES+=("$OUT/$name")
  done
else
  shopt -s nullglob
  EXES=("$OUT"/*.exe)
  shopt -u nullglob
fi

# Same carve-outs as scripts/run-micro-suite.sh: a persistent message loop or
# a stdin-blocking exe tells us nothing about coverage and would only hit the
# slice watchdog.
SKIP_NAMES=(
  snake.exe guess_price.exe
  gui_blit.exe gui_control.exe gui_d3d9.exe gui_demo.exe gui_dialog.exe
  gui_edit.exe gui_menu.exe gui_text.exe notepad_smoke.exe
  http_get.exe wininet_http.exe
  child_proc.exe spawn_child.exe
)
if [[ "$CPU" == "iced" ]]; then
  # Documented iced-only budget stop (see the header).
  SKIP_NAMES+=(long_loop.exe)
fi

skipped() {
  local name="$1" s
  for s in "${SKIP_NAMES[@]}"; do
    [[ "$s" == "$name" ]] && return 0
  done
  return 1
}

STDIN_FIX=""
stage=""
stage_d=""
# Per-exe watchdog. `timeout` is GNU coreutils; without it the harness still
# works, just without the hang guard.
PER_EXE_TIMEOUT="${WIE_COVERAGE_TIMEOUT:-120}"
TIMEOUT=()
if command -v timeout >/dev/null 2>&1; then
  TIMEOUT=(timeout "$PER_EXE_TIMEOUT")
fi
cleanup() {
  [[ -n "$STDIN_FIX" ]] && rm -f "$STDIN_FIX"
  [[ -n "$stage" ]] && rm -rf "$stage"
  [[ -n "$stage_d" ]] && rm -rf "$stage_d"
  return 0
}
trap cleanup EXIT

rows=()
suite_total=0
suite_jit=0
suite_iced=0
suite_degraded=0
suite_stops=0
suite_entries=0
n_exe=0

for exe in "${EXES[@]}"; do
  name="${exe##*/}"
  if skipped "$name"; then
    echo "--- skip $name ---"
    continue
  fi
  profile="$OUT/${name%.exe}.profile.txt"
  # The report itself is `eprintln!` (stderr), but the two histogram dumps are
  # `tracing::error!` (stdout) — keep both streams or the baseline loses the
  # exact interpreter counts.
  profile_out="$OUT/${name%.exe}.profile.stdout"
  target="$exe"
  args=()
  stage=""
  # Same per-exe staging as scripts/run-micro-suite.sh: these exes only exit
  # zero inside a throw-away bottle with their fixture staged, and a non-zero
  # exit here is a harness bug rather than a coverage datum.
  case "$name" in
    dll_*.exe)
      # DLL tests load siblings by bare name; name the whole out/ dir.
      args=(--app-dir "$OUT")
      ;;
    cli_args.exe) args=(-n 3 -m hi) ;;
    mbwc_lasterror.exe) ;;
    msvcrt_gaps.exe)
      # fgetwc/getc consume injected console stdin; without it the first
      # fgetwc hits EOF and the guest exits non-zero.
      STDIN_FIX="$(mktemp)"
      printf 'AB' >"$STDIN_FIX"
      args=(--stdin "$STDIN_FIX")
      ;;
    spawn_child.exe)
      # CreateProcess pair: the child embeds the compile-time path
      # C:\child_proc.exe, so both halves need one shared bottle.
      stage="$(mktemp -d)"
      mkdir -p "$stage/drive_c"
      cp "$OUT/child_proc.exe" "$stage/drive_c/child_proc.exe"
      args=(--root "$stage")
      ;;
    modules.exe)
      # Requires guest identity C:\App\modules.exe: run the in-bottle copy.
      stage="$(mktemp -d)"
      mkdir -p "$stage/drive_c/App"
      cp "$OUT/modules.exe" "$stage/drive_c/App/"
      target="$stage/drive_c/App/modules.exe"
      args=(--root "$stage")
      ;;
    read_file.exe)
      stage="$(mktemp -d)"
      mkdir -p "$stage/drive_c/App"
      printf 'hello-n2' >"$stage/drive_c/App/n2_in.txt"
      args=(--root "$stage")
      ;;
    vfs_roundtrip.exe)
      stage="$(mktemp -d)"
      stage_d="$(mktemp -d)"
      mkdir -p "$stage/drive_c/App"
      cp "$ROOT/micro-exes/vfs_roundtrip/fixture_utf8.txt" "$stage_d/vfs_in.txt"
      args=(--root "$stage" --drive-d "$stage_d")
      ;;
    write_file.exe)
      stage="$(mktemp -d)"
      mkdir -p "$stage/drive_c/App"
      args=(--root "$stage")
      ;;
  esac

  echo "--- run $name (WIE_CPU=$CPU) ---"
  set +e
  # Watchdog: a hang must be reported as a hang, not stall the whole
  # baseline. Generous — the slowest micro exe (long_loop) is <1 s of guest
  # time — so this only ever fires on a real deadlock.
  "${TIMEOUT[@]}" "$CLI" run "$target" "${args[@]}" >"$profile_out" 2>"$profile" </dev/null
  status=$?
  set -e
  if [[ $status -ne 0 ]]; then
    if [[ $status -eq 124 ]]; then
      echo "FAIL: $name hung (killed after ${PER_EXE_TIMEOUT}s) under WIE_CPU=$CPU" >&2
    else
      echo "FAIL: $name exited $status under WIE_CPU=$CPU (profile: $profile)" >&2
    fi
    tail -n 20 "$profile" >&2 || true
    [[ -n "$stage" ]] && rm -rf "$stage"
    [[ -n "${stage_d:-}" ]] && rm -rf "$stage_d"
    exit 1
  fi
  [[ -n "$stage" ]] && rm -rf "$stage"
  stage=""
  [[ -n "${stage_d:-}" ]] && rm -rf "$stage_d"
  stage_d=""
  if ! grep -q 'insn_coverage:' "$profile"; then
    echo "FAIL: $name produced no insn_coverage line (profile: $profile)" >&2
    exit 1
  fi

  cov_line="$(grep -m1 'insn_coverage:' "$profile")"
  caveat_line="$(grep -m1 'insn_coverage_caveat:' "$profile")"
  if [[ -z "$caveat_line" ]]; then
    echo "FAIL: $name produced insn_coverage without its caveat line" >&2
    exit 1
  fi
  cv() {
    # Read one key out of the insn_coverage line only — a bare `total=` grep
    # would also hit the histogram header's `total=…` and silently pick the
    # wrong number.
    local v
    v="$(printf '%s\n' "$cov_line" | grep -o "$1=[^ ]*" | head -1 | cut -d= -f2)"
    printf '%s' "${v:-0}"
  }
  total="$(cv total)"
  jit="$(cv jit)"
  iced="$(cv iced)"
  share="$(cv jit_share_pct)"
  degraded="$(cv degraded)"
  per_entry="$(cv insn_per_entry)"
  entries="$(cv block_entries)"
  stops="$(grep -o 'host_stops=[0-9]*' "$profile" | tail -1 | cut -d= -f2)"
  rows+=("$(printf '%-22s %12s %6s %6s %9s %10s %10s' \
    "${name%.exe}" "$total" "$share" "$iced" "$degraded" "${stops:-0}" "$per_entry")")

  suite_total=$((suite_total + total))
  suite_jit=$((suite_jit + jit))
  suite_iced=$((suite_iced + iced))
  suite_degraded=$((suite_degraded + degraded))
  suite_entries=$((suite_entries + entries))
  suite_stops=$((suite_stops + ${stops:-0}))
  n_exe=$((n_exe + 1))
done

if [[ $n_exe -eq 0 ]]; then
  echo "FAIL: no exes were profiled" >&2
  exit 1
fi

# ----------------------------------------------------- opcode histograms ---
# Two blocks are aggregated, both rendered by the runtime's own
# `render_mnemonic_histogram` (never re-implemented here):
#   1. `--- jit iced-residue opcode histogram ... ---` — the JIT's 1/64 sample of
#      interpreted steps, inside the profile report (stderr).
#   2. `--- iced-interp mnemonic counts ... ---` — the *unsampled* interpreter
#      counters, i.e. exact counts of every interpreted instruction (stdout).
# Rows are "<count>  <pct>%  <Mnemonic>"; each line carries a
# "<ts> ERROR <target>: " tracing prefix, so the row is taken from a regex match
# rather than a fixed column. Percentages are recomputed against the summed
# grand total, so the suite-wide ranking is a real frequency and not an average
# of per-exe percentages.
hist_tmp="$(mktemp -d)"
trap 'rm -rf "$hist_tmp"; cleanup' EXIT

extract_hist() { # extract_hist <header-regex> <file-glob> <out>
  local header="$1" out="$3" p
  for p in $2; do
    [[ -f "$p" ]] || continue
    awk -v hdr="$header" '
      $0 ~ hdr { inblock = 1; next }
      inblock && /--- end ---/ { inblock = 0; next }
      inblock {
        # Match "<count> <pct>% <Mnemonic>" at end of line, ignore any prefix.
        if (match($0, /[0-9]+[ \t]+[0-9]+\.[0-9]%[ \t]+[A-Za-z_][A-Za-z0-9_]*[ \t]*$/)) {
          row = substr($0, RSTART)
          split(row, f, /[ \t]+/)
          printf "%s %s\n", f[1], f[3]
        }
      }' "$p" >>"$out"
  done
}

# Sum the per-exe counts of each mnemonic and render against the grand total.
render_hist() { # render_hist <rows-file>
  [[ -s "$1" ]] || return 0
  local grand
  grand="$(awk '{ s += $1 } END { print s + 0 }' "$1")"
  awk '{ total[$2] += $1 } END { for (k in total) printf "%d %s\n", total[k], k }' "$1" |
    sort -k1,1nr |
    awk -v grand="$grand" '
      { tenths = int($1 * 1000 / grand);
        printf "  %10d  %5d.%d  %s\n", $1, int(tenths / 10), tenths % 10, $2 }'
}

extract_hist "jit iced-residue opcode histogram" "$OUT/*.profile.txt" "$hist_tmp/jit_rows.txt"
extract_hist "iced-interp mnemonic counts" "$OUT/*.profile.stdout" "$hist_tmp/ice_rows.txt"

pct_of() { # pct_of <count> <total> → tenths of a percent
  awk -v c="$1" -v t="$2" 'BEGIN { if (t == 0) print "0.0"; else printf "%d.%d", int(c*1000/t)/10, (c*1000/t)%10 }'
}

hist_body="$(render_hist "$hist_tmp/jit_rows.txt")"
hist_body_ice="$(render_hist "$hist_tmp/ice_rows.txt")"
jit_samples="$(awk '{ s += $1 } END { print s + 0 }' "$hist_tmp/jit_rows.txt")"
ice_insns="$(awk '{ s += $1 } END { print s + 0 }' "$hist_tmp/ice_rows.txt")"

# ----------------------------------------------------------------- append ---
suite_share="$(pct_of "$suite_jit" "$suite_total")"
suite_deg_pct="$(pct_of "$suite_degraded" "$suite_total")"
suite_per_entry="$(pct_of "$suite_total" "$suite_entries")"
stamp="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

# Caveat text is copied verbatim out of a real run's report so the baseline can
# never describe the metric more charitably than the runtime does.
CAVEAT="$(printf '%s\n' "$caveat_line" | sed 's/^insn_coverage_caveat: //')"

{
  echo "# WIE instruction-coverage baseline — appended by scripts/coverage-report.sh"
  echo "# $stamp | WIE_CPU=$CPU | exes=$n_exe | total_insns=$suite_total | jit=$suite_jit | iced=$suite_iced | jit_share=${suite_share}% | degraded=$suite_degraded (${suite_deg_pct}%) | host_stops=$suite_stops"
  echo "#"
  echo "# CAVEAT (verbatim from the runtime report): $CAVEAT"
  echo "# The jit/iced columns above are DYNAMIC retired-instruction counts"
  echo "# (basis=dynamic_retired), with guest worker-thread engines merged in."
  echo "# The one known undercount is a REP string helper (1 instruction, not rcx"
  echo "# iterations), so a rep movs*-dominated row is a lower bound."
  echo "# The two histograms below are exact (both count interpreted steps)."
  echo "#"
  printf '%-22s %12s %6s %6s %9s %10s %10s\n' exe total_insns jit% iced degraded host_stops insn/entry
  printf '%s\n' "${rows[@]}"
  printf '%-22s %12s %6s %6s %9s %10s %10s\n' "SUITE($CPU)" "$suite_total" "$suite_share" "$suite_iced" "$suite_degraded" "$suite_stops" "$suite_per_entry"
  echo
  echo "# suite-wide JIT iced-residue opcode histogram (sampled 1/64, $jit_samples samples over $n_exe exes):"
  if [[ -n "$hist_body" ]]; then
    printf '%s\n' "$hist_body"
  else
    echo "  (no sampled residue — every profiled block was JIT-compiled)"
  fi
  echo
  echo "# suite-wide interpreter opcode mix (EXACT: $ice_insns interpreted instructions):"
  if [[ -n "$hist_body_ice" ]]; then
    printf '%s\n' "$hist_body_ice"
  else
    echo "  (no interpreted instructions at all)"
  fi
  echo
} >>"$BASELINE"

echo "=== coverage: $n_exe exes, WIE_CPU=$CPU ==="
echo "# CAVEAT: $CAVEAT"
printf '%-22s %12s %6s %6s %9s %10s %10s\n' exe total_insns jit% iced degraded host_stops insn/entry
printf '%s\n' "${rows[@]}"
printf '%-22s %12s %6s %6s %9s %10s %10s\n' "SUITE($CPU)" "$suite_total" "$suite_share" "$suite_iced" "$suite_degraded" "$suite_stops" "$suite_per_entry"
echo
echo "# suite-wide interpreter opcode mix (EXACT: $ice_insns interpreted instructions):"
if [[ -n "$hist_body_ice" ]]; then
  printf '%s\n' "$hist_body_ice"
else
  echo "  (no interpreted instructions at all)"
fi
echo
echo "appended baseline: ${BASELINE#"$ROOT/"}"
