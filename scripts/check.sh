#!/usr/bin/env bash
# Pre-PR checklist and the single source of truth for what the gate runs:
# the two policy checks, fmt, clippy, the unit/integration tests and the
# end-to-end micro-suite. Exits non-zero on first failure.
#
# Callers:
#   README.md / CONTRIBUTING.md      ./scripts/check.sh
#   .githooks/pre-commit             one cheap policy step (--only)
#   bacon.toml                       job "full-check" (all) and "s" (--only)
#   .github/workflows/ci.yml         the same steps, split across named CI steps
#   .github/workflows/release.yml    build-release-pgo.sh (unrelated)
#
# Usage:
#   scripts/check.sh                    every step, in the canonical order
#   scripts/check.sh --only <step>      just that step (repeatable)
#   scripts/check.sh --list             print the step names and exit
#
# `--only` exists for the callers that want one cheap policy check without the
# build/test cost behind it (the pre-commit hook, bacon's per-keystroke job, the
# two separately-named ci.yml steps). The policy checks are *functions below*,
# not separate scripts, so there is exactly one copy of each rule; a `--only`
# call runs the very same function the full gate runs.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# The canonical order. A space-separated string, not an array: bash 3.2 (the
# macOS system bash) errors on expanding an empty array under `set -u`, and the
# other two scripts this one absorbed had that hazard documented.
ALL_STEPS="file-sizes dep-direction fmt clippy micro-exes nextest micro-suite perf"

usage() {
  cat <<EOF
usage: scripts/check.sh [--only <step>]... [--list]

Runs the pre-PR gate. Steps, in order:
$(printf '    %s\n' $ALL_STEPS)
EOF
}

# ---------------------------------------------------------------------------
# Step 1 — file-size policy (ADR-002)
#
#   - hard cap: 2000 lines per source file
#   - target:   <= 1000 lines per source file
# Exemptions: test files (tests.rs and anything under a tests/ directory) and
# pure data tables (see DATA_TABLES below; currently none). Exits non-zero when
# a non-exempt file exceeds the hard cap.
#
# First in the gate: it is ~1s, needs no toolchain, and fails on the change that
# is cheapest to fix (a file that grew).
# ---------------------------------------------------------------------------
check_file_sizes() {
  local HARD_CAP=2000

  # Pure data tables that legitimately exceed the cap (ADR-002 exception).
  # Keep this list minimal and commented; prefer splitting over extending it.
  # Intentionally empty: the only entry this list ever had pointed at
  # `dispatch_table/names.rs`, which is now `dispatch_table/names/mod.rs` and is
  # under the hard cap anyway. Add a row here only for a file that really is over
  # HARD_CAP and really is pure data.
  local DATA_TABLES=()

  local fail=0 f rel t lines
  while IFS= read -r -d '' f; do
    rel="${f#"$ROOT"/}"
    case "$rel" in
      */tests.rs|*/tests/*) continue ;; # test files exempt
    esac
    # `set -u` + an empty array is an unbound-variable error on bash 3.2 (the
    # macOS system bash), so skip the loop when the list is empty.
    if (( ${#DATA_TABLES[@]} > 0 )); then
      for t in "${DATA_TABLES[@]}"; do
        if [[ "$rel" == "$t" ]]; then
          continue 2
        fi
      done
    fi
    lines=$(wc -l < "$f" | tr -d ' ')
    if (( lines > HARD_CAP )); then
      echo "OVER CAP ($lines > $HARD_CAP): $rel"
      fail=1
    fi
  done < <(find "$ROOT/crates" -name '*.rs' -not -path '*/target/*' -print0)

  if (( fail )); then
    echo "file-size policy violated (cap $HARD_CAP lines, see check_file_sizes() in scripts/check.sh)"
    exit 1
  fi
  echo "file sizes ok (hard cap $HARD_CAP)"
}

# ---------------------------------------------------------------------------
# Step 2 — workspace crate dependency direction (CLAUDE.md "Architecture":
#   wie-pe -> wie-cpu -> wie-winapi -> wie-runtime -> wie-cli)
#
# Nothing in the test suite asserts this: when this check was added there were 0
# architecture-boundary tests, so a new `use` or a new `path` entry could invert
# the layering and all 1,884 tests would still pass. This makes the layering
# explicit and machine-checked.
#
# A deliberate architecture change is a ONE-LINE edit: update EXPECTED_EDGES
# below. Entries are "from -> to" for NORMAL dependencies. Workspace-internal
# dev-dependencies are held to a separate (currently empty) list because a lower
# layer's tests reaching upward is a different, equally real violation.
#
# Out of scope: external crates — `criterion` (dev-dep of wie-cpu) and the
# platform-gated macOS deps in wie-cli (muda, rfd, objc2*, wgpu, pollster,
# bytemuck). This check is about *internal* workspace edges only, so a new third-
# party crate never needs an entry here.
#
# Accepted deviation, encoded deliberately below: `wie-cli` depends on
# `wie-winapi` directly and reaches around `wie-runtime`
# (crates/wie-cli/src/main.rs calls console::profile_sigint_armed() /
# ensure_hooks_installed()). That lets the CLI call a WinAPI handler without
# RuntimeSession's lock and in-guest-callback invariants. It is listed in
# EXPECTED_EDGES so a future reader sees it as known, not as drift. Revisit it by
# editing this list, not by silencing the check.
#
# Before the expensive build steps: it costs ~1s (cargo metadata, no compile)
# and is the only check that fails on an architecture change, so it should stop
# the gate before anything expensive starts. Exits non-zero if the observed
# internal edge set differs from the expected one.
# ---------------------------------------------------------------------------
check_deps() {
  # The `cd` is inside the function-subshell below (see extract_edges), not here:
  # a bare `cd` in a function would change the cwd of the whole gate.
  #
  # -------------------------------------------------------------------------
  # EXPECTED INTERNAL EDGE SET — the single source of truth.
  # One line here = one deliberate architecture change.
  # -------------------------------------------------------------------------
  local EXPECTED_EDGES=(
    "wie-cli -> wie-pe"
    "wie-cli -> wie-runtime"
    "wie-cli -> wie-winapi"
    "wie-runtime -> wie-cpu"
    "wie-runtime -> wie-pe"
    "wie-runtime -> wie-winapi"
    "wie-winapi -> wie-cpu"
    "wie-winapi -> wie-pe"
  )

  # Expected workspace-internal *dev*-dependency edges ("from -> to"). Empty by
  # design: every crate is a leaf in the layering, so a lower layer's tests must
  # not reach into a higher one (e.g. wie-cpu dev-depending on wie-runtime would
  # make wie-cpu's test build depend on the whole stack above it).
  local EXPECTED_DEV_EDGES=()

  # `cargo metadata` needs a toolchain; ci.yml, the pre-commit hook and the rest
  # of this script set one up before this runs. Run it twice and split the two
  # edge classes out of it; `--quiet` keeps cargo's "Updating crates.io index"
  # chatter off the failure output. The subshell form (`name() ( ... )`) keeps
  # the `cd` scoped: this used to be a whole script, where the `cd` was free.
  extract_edges() ( # $1 = "normal" | "dev"
    cd "$ROOT"
    local want="$1"
    cargo metadata --quiet --format-version 1 --no-deps | CRATE_EDGE_KIND="$want" python3 -c '
import json, os, sys
want = os.environ["CRATE_EDGE_KIND"]
md = json.load(sys.stdin)
rows = []
for p in md["packages"]:
    src = p["name"]
    for d in p["dependencies"]:
        if not d.get("path"):
            continue  # external crate: out of scope (criterion, wgpu, objc2*, ...)
        if (d.get("kind") or "normal") == want:
            rows.append(src + " -> " + d["name"])
print("\n".join(sorted(rows)))
'
  )

  # One row per line. Never `printf '%s\n' $rows` unquoted: that word-splits
  # "a -> b" into three tokens and prints the arrow on its own line.
  print_rows() {
    while IFS= read -r row; do
      [[ -n "$row" ]] && printf '  %s\n' "$row"
    done
  }

  local expected_sorted expected_dev_sorted observed_sorted observed_dev_sorted
  local extra missing extra_dev missing_dev n fail

  expected_sorted=$(printf '%s\n' "${EXPECTED_EDGES[@]}" | sed '/^$/d' | sort)
  expected_dev_sorted=$(printf '%s\n' "${EXPECTED_DEV_EDGES[@]:-}" | sed '/^$/d' | sort)
  observed_sorted=$(extract_edges normal | sed '/^$/d' | sort)
  observed_dev_sorted=$(extract_edges dev | sed '/^$/d' | sort)

  fail=0

  extra=$(comm -13 <(printf '%s\n' "$expected_sorted") <(printf '%s\n' "$observed_sorted"))
  if [[ -n "$extra" ]]; then
    echo "UNEXPECTED internal dependency edge(s) [normal] (new, not in EXPECTED_EDGES in check_deps() in scripts/check.sh):"
    printf '%s\n' "$extra" | print_rows
    fail=1
  fi

  missing=$(comm -23 <(printf '%s\n' "$expected_sorted") <(printf '%s\n' "$observed_sorted"))
  if [[ -n "$missing" ]]; then
    echo "MISSING internal dependency edge(s) [normal] (removed; the layer chain documented in CLAUDE.md no longer holds):"
    printf '%s\n' "$missing" | print_rows
    fail=1
  fi

  # Reported separately from normal deps so the message says which class moved.
  # An internal dev-dep that is not in EXPECTED_DEV_EDGES is drift.
  extra_dev=$(comm -13 <(printf '%s\n' "$expected_dev_sorted") <(printf '%s\n' "$observed_dev_sorted"))
  if [[ -n "$extra_dev" ]]; then
    echo "UNEXPECTED internal dependency edge(s) [dev] (a lower layer must not dev-depend on a higher one; no internal dev-deps are expected):"
    printf '%s\n' "$extra_dev" | print_rows
    fail=1
  fi

  missing_dev=$(comm -23 <(printf '%s\n' "$expected_dev_sorted") <(printf '%s\n' "$observed_dev_sorted"))
  if [[ -n "$missing_dev" ]]; then
    echo "MISSING internal dependency edge(s) [dev] (was listed in EXPECTED_DEV_EDGES and has been removed):"
    printf '%s\n' "$missing_dev" | print_rows
    fail=1
  fi

  if (( fail )); then
    echo "dependency-direction policy violated (see check_deps() in scripts/check.sh)"
    echo "to accept an architecture change, edit EXPECTED_EDGES at the top of check_deps()."
    exit 1
  fi

  n=$(printf '%s\n' "$observed_sorted" | sed '/^$/d' | wc -l | tr -d ' ')
  echo "crate dependency direction ok ($n internal normal edges, ${#EXPECTED_EDGES[@]} expected, 0 internal dev edges)"
}

# ---------------------------------------------------------------------------
# Step 8 — `perf` budget: long_loop wall-clock.
#
# CONTRIBUTING rule 2 makes the long_loop pin a pass/fail condition, so it needs
# a gate rather than a habit. Measured 2026-10-01 on a quiet host via the
# canonical suite path: 0.43/0.47/0.50 s (min/median/max, n=5), release JIT with
# the default `WIE_JIT_OPT=none`.
#
# Measured through `run-micro-suite.sh exe long_loop`, NOT by invoking
# `wie run` on the exe directly: that path also stages the exe into the bottle
# and reads ~0.7 s, which would fail this budget for a reason that has nothing
# to do with the JIT.
#
# Best-of-N, and the MEDIAN decides: interference only ever adds time, so a
# single slow run is host noise rather than a regression. `LONG_LOOP_MAX` is the
# budget's ceiling, deliberately the loose end of the documented 0.40-0.55 s
# range — a change that only trips the ceiling has genuinely slowed the guest.
#
# Not in CI: this is a timing gate, so it is meaningless on a shared runner where
# load varies run to run. Run it locally before opening a PR.
# ---------------------------------------------------------------------------
check_perf_budget() {
  local LONG_LOOP_MAX=0.55   # seconds
  local RUNS=5
  local SETTLE=30           # seconds; let a just-finished micro-suite decay
  # long_loop is single-threaded and burns one core by design, so any *other*
  # runnable work shows up directly in its wall time. Measured on this host:
  # ~0.26 s median at load ~0.5, ~0.5-0.7 s at load ~8. A budget check that
  # reports a number it knows is contaminated is worse than no check — it fails
  # PRs for reasons that have nothing to do with the code. Decline to measure.
  local MAX_LOAD="${WIE_PERF_MAX_LOAD:-2.0}"

  if [[ ! -x "$ROOT/target/release/wie" ]]; then
    echo "perf: SKIPPED — no release binary; run 'cargo build -p wie-cli --release' first"
    return 0
  fi

  local load
  load=$(sysctl -n vm.loadavg 2>/dev/null | tr -d '{}' | awk '{print $1}') || load=""
  if [[ -n "$load" ]] && awk "BEGIN {exit !($load > $MAX_LOAD)}"; then
    echo "perf: SKIPPED — host too loaded (1-min load ${load} > ${MAX_LOAD})."
    echo "  long_loop is single-threaded, so background work lands directly in its"
    echo "  wall time; measuring now would report the host, not the JIT. Close the"
    echo "  other work and re-run, or override with WIE_PERF_MAX_LOAD=<n>."
    return 0
  fi
  [[ -n "$load" ]] && echo "perf: host load ${load} (limit ${MAX_LOAD}) — proceeding"

  # A stale binary produces a plausible-looking number that measures nothing. That
  # is the worst failure mode for a timing gate, so check freshness loudly rather
  # than silently timing whatever happens to be in target/release.
  #
  # Test-only and bench files are excluded: they are `#[cfg(test)]`/`bench`-gated,
  # so they cannot change release codegen, and including them would make this
  # warning fire after almost any edit and train readers to ignore it. Match any
  # directory ending in `tests` (`tests/`, `paint_tests/`, …) plus `benches/`.
  local newer
  newer=$(find "$ROOT/crates" -name '*.rs' -newer "$ROOT/target/release/wie" \
            -not -path '*/tests/*' -not -path '*_tests/*' \
            -not -name '*_tests.rs' -not -name 'tests.rs' \
            -not -path '*/benches/*' -print -quit 2>/dev/null || true)
  if [[ -n "$newer" ]]; then
    echo "perf: WARNING — release binary is older than ${newer#$ROOT/}"
    echo "  timing a stale binary measures the wrong code; rebuild first:"
    echo "    cargo build -p wie-cli --release"
  fi

  # This step runs LAST, immediately after ~213 guest fixtures. Load average
  # decays over minutes, so a measurement taken now is contaminated no matter how
  # many samples it takes. Settle first.
  echo "perf: settling ${SETTLE}s before measuring (the micro-suite just loaded this host)"
  sleep "$SETTLE"

  # No `local -n` (nameref) here: that is bash 4.3+, and this script must also run
  # under macOS's /bin/bash 3.2. Results come back in the global PERF_SAMPLES.
  measure() {
    PERF_SAMPLES=()
    local i
    for ((i = 0; i < RUNS; i++)); do
      t=$( { /usr/bin/time -p "$ROOT/scripts/run-micro-suite.sh" exe long_loop; } 2>&1 \
          | awk '/^real/ {print $2; exit}' )
      if [[ -z "$t" ]]; then
        echo "perf: FAILED — could not time long_loop"
        echo "  check that micro-exes/out/long_loop.exe is built (make -C micro-exes)"
        echo "  and that target/release/wie exists (cargo build -p wie-cli --release)"
        return 1
      fi
      PERF_SAMPLES+=("$t")
    done
    return 0
  }

  # Median of the samples, after discarding gross cold-start outliers: the first
  # run can read an order of magnitude slow (page cache, CPU frequency ramp).
  PERF_SAMPLES=()
  measure || return 1
  local samples=("${PERF_SAMPLES[@]}")

  local sorted min kept=() median k
  sorted=$(printf '%s\n' "${samples[@]}" | sort -n)
  min=$(printf '%s\n' "$sorted" | head -1)
  while read -r k; do
    if awk "BEGIN {exit !($k < $min * 10)}"; then kept+=("$k"); fi
  done <<< "$sorted"
  median=$(printf '%s\n' "${kept[@]}" | sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}')

  echo "perf: long_loop median ${median}s over ${#kept[@]} samples (budget ${LONG_LOOP_MAX}s)"
  echo "  samples: ${samples[*]}"

  if ! awk "BEGIN {exit !($median > $LONG_LOOP_MAX)}"; then
    echo "perf budget ok (long_loop median ${median}s)"
    return 0
  fi

  # Over budget: interference only ever adds time, and this host has just run the
  # whole suite. Settle again and re-measure before believing a regression.
  echo "perf: over budget — settling ${SETTLE}s and re-measuring before failing"
  sleep "$SETTLE"
  PERF_SAMPLES=()
  measure || return 1
  local retry=("${PERF_SAMPLES[@]}")
  local rmedian
  rmedian=$(printf '%s\n' "${retry[@]}" | sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}')
  echo "perf: re-measured median ${rmedian}s (samples: ${retry[*]})"

  if awk "BEGIN {exit !($rmedian > $LONG_LOOP_MAX)}"; then
    echo "perf budget violated: long_loop median ${rmedian}s > ${LONG_LOOP_MAX}s on two runs"
    echo "  host is still loaded if the load average is high — check before"
    echo "  treating this as a code regression; see docs/RUNBOOK.md."
    return 1
  fi
  echo "perf budget ok on re-measure (${rmedian}s) — the first reading was host noise"
}

# ---------------------------------------------------------------------------
# Steps 3-7. `micro-exes` MUST precede `nextest`: the integration tests under
# `crates/wie-runtime/tests/micro_gui_window/` exec the guest PEs that
# `micro-exes/out` produces. Built afterwards, they fail on a clean tree with
# "no such file", which reads like a code defect rather than a missing fixture.
# That ordering was a real bug fix; do not regress it.
# ---------------------------------------------------------------------------
step_header() {
  case "$1" in
    file-sizes)    echo "=== file sizes ===" ;;
    dep-direction) echo "=== crate dependency direction ===" ;;
    fmt)           echo "=== cargo fmt ===" ;;
    clippy)        echo "=== cargo clippy (advisory; no -D warnings — clippy lints are not denied) ===" ;;
    micro-exes)    echo "=== micro-exes (fixtures for the integration tests) ===" ;;
    nextest)       echo "=== cargo nextest ===" ;;
    micro-suite)   echo "=== micro-suite ===" ;;
    perf)          echo "=== perf budget: long_loop ===" ;;
  esac
}

run_step() {
  case "$1" in
    file-sizes)    check_file_sizes ;;
    dep-direction) check_deps ;;
    fmt)           cargo fmt --all --check --manifest-path "$ROOT/Cargo.toml" ;;
    clippy)        cargo clippy --workspace --all-targets --manifest-path "$ROOT/Cargo.toml" ;;
    micro-exes)    make -C "$ROOT/micro-exes" ;;
    nextest)       cargo nextest run --workspace --manifest-path "$ROOT/Cargo.toml" ;;
    micro-suite)   "$ROOT/scripts/run-micro-suite.sh" ;;
    perf)          check_perf_budget ;;
  esac
}

# --- argument parsing ------------------------------------------------------
# Empty until a --only is seen, so the first --only narrows rather than adds to
# an already-complete selection.
selected=""
ran_all=1

while (( $# )); do
  case "$1" in
    --list)
      printf '%s\n' $ALL_STEPS
      exit 0
      ;;
    --only)
      shift
      if (( $# == 0 )); then
        echo "check.sh: --only needs a step name; see --list" >&2
        exit 2
      fi
      if [[ " $ALL_STEPS " != *" $1 "* ]]; then
        echo "check.sh: unknown step: $1" >&2
        usage >&2
        exit 2
      fi
      case " $selected " in
        *" $1 "*) ;; # already selected; keep it once
        *) selected="${selected:+$selected }$1" ;;
      esac
      ran_all=0
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "check.sh: unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
  shift
done

if (( ran_all )); then
  selected="$ALL_STEPS"
fi

# Always execute in the canonical order, whatever order --only was given in, so
# `--only nextest --only micro-exes` cannot build the fixtures after the tests.
for step in $ALL_STEPS; do
  case " $selected " in
    *" $step "*)
      step_header "$step"
      run_step "$step"
      ;;
  esac
done

if (( ran_all )); then
  echo "=== all checks passed ==="
else
  echo "=== selected steps passed: $selected ==="
fi
