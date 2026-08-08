#!/usr/bin/env bash
# Enforce the file-size policy (ADR-002):
#   - hard cap: 1500 lines per source file
#   - target:   <= 1000 lines per source file
# Exemptions:
#   - test files (tests.rs and anything under a tests/ directory)
#   - pure data tables (e.g. dispatch_table/names.rs — the WinApiId name rows)
# Exits non-zero when a non-exempt file exceeds the hard cap.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HARD_CAP=1500

# Pure data tables that legitimately exceed the cap (ADR-002 exception).
# Keep this list minimal and commented; prefer splitting over extending it.
DATA_TABLES=(
  "crates/wie-winapi/src/dispatch_table/names.rs"
)

fail=0
while IFS= read -r -d '' f; do
  rel="${f#"$ROOT"/}"
  case "$rel" in
    */tests.rs|*/tests/*) continue ;; # test files exempt
  esac
  for t in "${DATA_TABLES[@]}"; do
    if [[ "$rel" == "$t" ]]; then
      continue 2
    fi
  done
  lines=$(wc -l < "$f" | tr -d ' ')
  if (( lines > HARD_CAP )); then
    echo "OVER CAP ($lines > $HARD_CAP): $rel"
    fail=1
  fi
done < <(find "$ROOT/crates" -name '*.rs' -not -path '*/target/*' -print0)

if (( fail )); then
  echo "file-size policy violated (cap $HARD_CAP lines, see scripts/check-file-sizes.sh)"
  exit 1
fi
echo "file sizes ok (hard cap $HARD_CAP)"
