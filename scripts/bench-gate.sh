#!/usr/bin/env bash
# Criterion regression gate for wie-cpu benches.
#
# Usage:
#   ./scripts/bench-gate.sh save <label> [bench-filter]
#       Run all benches (or those matching filter) and store results as a
#       named baseline. Use on a known-good tree (e.g. main).
#
#   ./scripts/bench-gate.sh check <label> [threshold_pct] [bench-filter]
#       Re-run benches against the saved baseline and FAIL (exit 1) when any
#       case's median time regressed more than threshold_pct (default 10).
#       Improvements are reported but never fail.
#
# Examples:
#   ./scripts/bench-gate.sh save main
#   git checkout -b my-change
#   ./scripts/bench-gate.sh check main          # gate at default 10%
#   ./scripts/bench-gate.sh check main 5 exec/  # only exec/* cases, tighter
#
# Notes:
# - Criterion prints one "change:" line per case as
#     change: [x% y% z%] (p = …)
#   where y% is the median delta vs the baseline. Only the median is gated;
#   noise on the tails is ignored by design.
# - First check run after `save` shows large changes if the machine differs;
#   baselines are per-machine, like criterion's own automatic comparison.
set -euo pipefail

MODE="${1:?usage: bench-gate.sh save|check <label> [arg] [filter]}"
LABEL="${2:?missing baseline label}"
FILTER="${4:-}"

run_benches() {
    local extra=("$@")
    cargo bench -p wie-cpu -- "${extra[@]}"
}

case "$MODE" in
save)
    echo "== saving baseline '$LABEL'" ${FILTER:+"(filter: $FILTER)"} "=="
    run_benches --save-baseline "$LABEL" ${FILTER:+"$FILTER"}
    echo "Baseline '$LABEL' stored under target/criterion."
    ;;
check)
    THRESHOLD="${3:-10}"
    echo "== checking against baseline '$LABEL' (threshold ${THRESHOLD}%)" \
        ${FILTER:+"(filter: $FILTER)"} "=="
    OUT="$(mktemp /tmp/wie-bench-check.XXXXXX.txt)"
    # shellcheck disable=SC2086
    run_benches --baseline "$LABEL" ${FILTER:+"$FILTER"} | tee "$OUT"

    FAILURES=0
    CASES=0
    CURRENT=""
    # Criterion interleaves "name" lines with their "change:" lines; keep the
    # most recent name seen so failures are attributable.
    while IFS= read -r line; do
        if [[ "$line" =~ ^[a-z_]+/ ]]; then
            CURRENT="$line"
        elif [[ "$line" == *"change:"* && "$line" == *"%"* ]]; then
            # Extract the median (second percentage inside the brackets).
            MEDIAN="$(sed -E 's/.*change: \[[-0-9.]+% ([-0-9.]+)%.*/\1/' <<<"$line")"
            if [[ -z "$MEDIAN" ]]; then
                continue
            fi
            CASES=$((CASES + 1))
            # awk float compare without bc.
            REGRESSED="$(awk -v m="$MEDIAN" -v t="$THRESHOLD" 'BEGIN{print (m>t)?1:0}')"
            if [[ "$REGRESSED" == "1" ]]; then
                echo "REGRESSION: $CURRENT median ${MEDIAN}% > ${THRESHOLD}%"
                FAILURES=$((FAILURES + 1))
            fi
        fi
    done <"$OUT"

    rm -f "$OUT"
    if [[ "$CASES" -eq 0 ]]; then
        echo "No comparison data found — did baseline '$LABEL' exist?" >&2
        exit 2
    fi
    if [[ "$FAILURES" -gt 0 ]]; then
        echo "bench-gate: $FAILURES/$CASES case(s) regressed beyond ${THRESHOLD}%" >&2
        exit 1
    fi
    echo "bench-gate: OK ($CASES cases within ${THRESHOLD}% of '$LABEL')"
    ;;
*)
    echo "unknown mode: $MODE (use save|check)" >&2
    exit 2
    ;;
esac
