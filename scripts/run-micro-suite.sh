#!/usr/bin/env bash
# Run micro-EXE test gates by category. Exit non-zero on first failure.
# Usage: ./run-micro-suite.sh [category]
#   category: all (default), cpp_exes, seh_exes, game, dll_tests
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CLI="${CLI:-$ROOT/target/release/wie-cli}"
CPU="${WIE_CPU:-jit}"
CATEGORY="${1:-all}"

if [[ ! -x "$CLI" ]]; then
  echo "building wie-cli (release)…"
  cargo build -p wie-cli --release --manifest-path "$ROOT/Cargo.toml"
fi

export WIE_CPU="$CPU"
echo "=== micro-suite: ${CATEGORY} (WIE_CPU=$WIE_CPU) ==="

run_one() {
  local pe="$1"
  shift
  echo "--- run $pe $* ---"
  "$CLI" run "$pe" "$@"
}

run_bottle_n2() {
  local pe="$1"
  local root="$2"
  shift 2
  run_one "$pe" --root "$root" "$@"
}

# Build only the requested category
make -C "$ROOT/micro-exes" "${CATEGORY}"

OUT="$ROOT/micro-exes/out"

if [[ "$CATEGORY" == "all" || "$CATEGORY" == "n1" ]]; then
  # N1 — no bottle required
  run_one "$OUT/process_ids.exe"
  run_one "$OUT/tls_basic.exe"
  run_one "$OUT/cs_reenter.exe"
  run_one "$OUT/thread_create_join.exe"
  run_one "$OUT/cs_two_threads.exe"
  run_one "$OUT/event_handshake.exe"
  run_one "$OUT/interlocked_basic.exe"
  run_one "$OUT/heap_alloc.exe"
  run_one "$OUT/heap_core.exe"
  run_one "$OUT/winapi_heap.exe"
  run_one "$OUT/modules.exe"
  run_one "$OUT/cpu_string.exe"
  run_one "$OUT/cpu_math.exe"
  run_one "$OUT/cpu_fp.exe"
  run_one "$OUT/crt_hello.exe"
  if [[ "${WIE_MT:-1}" != "0" ]]; then
    run_one "$OUT/mt_stress.exe"
  else
    echo "--- skip mt_stress (WIE_MT=0) ---"
  fi
  if [[ "${WIE_SKIP_LONG_LOOP:-0}" != "1" ]]; then
    run_one "$OUT/long_loop.exe"
  else
    echo "--- skip long_loop (WIE_SKIP_LONG_LOOP=1) ---"
  fi

  # Pseudo-CLI: flags + stdin
  CLI_STDIN="$(mktemp "${TMPDIR:-/tmp}/wie-cli-stdin.XXXXXX")"
  printf 'CLI_IN\n' >"$CLI_STDIN"
  run_one "$OUT/cli_args.exe" --stdin "$CLI_STDIN" -- -n 3 -m hi -i
  rm -f "$CLI_STDIN"
  echo "--- run cli_args.exe (live pipe stdin) ---"
  printf 'hello-live\n' | "$CLI" run "$OUT/cli_args.exe" -- -n 3 -m hi -i
fi

if [[ "$CATEGORY" == "all" || "$CATEGORY" == "n2" ]]; then
  # N2 — bottle v0
  BOTTLE="$(mktemp -d "${TMPDIR:-/tmp}/wie-bottle.XXXXXX")"
  mkdir -p "$BOTTLE/drive_c/App"
  printf 'hello-n2' >"$BOTTLE/drive_c/App/n2_in.txt"
  echo "bottle: $BOTTLE"

  run_bottle_n2 "$OUT/write_file.exe" "$BOTTLE"
  [[ -f "$BOTTLE/drive_c/App/n2_out.txt" ]] || { echo "FAIL: n2_out.txt missing"; exit 1; }
  grep -q 'WIE_N2' "$BOTTLE/drive_c/App/n2_out.txt" || { echo "FAIL: n2_out.txt content"; exit 1; }

  run_bottle_n2 "$OUT/read_file.exe" "$BOTTLE"
  run_bottle_n2 "$OUT/relative_path.exe" "$BOTTLE"
  [[ -f "$BOTTLE/drive_c/App/n2_rel_out.txt" ]] || { echo "FAIL: n2_rel_out.txt missing"; exit 1; }
  grep -q 'REL_OK' "$BOTTLE/drive_c/App/n2_rel_out.txt" || { echo "FAIL: n2_rel_out.txt content"; exit 1; }

  # VFS round-trip with drive D:
  DRIVE_D="$(mktemp -d "${TMPDIR:-/tmp}/wie-drive-d.XXXXXX")"
  FIXTURE="$ROOT/micro-exes/vfs_roundtrip/fixture_utf8.txt"
  cp "$FIXTURE" "$DRIVE_D/vfs_in.txt"
  echo "drive_d: $DRIVE_D"
  run_bottle_n2 "$OUT/vfs_roundtrip.exe" "$BOTTLE" --drive-d "$DRIVE_D"
  [[ -f "$BOTTLE/drive_c/App/vfs_copy.txt" ]] || { echo "FAIL: vfs_copy.txt missing"; exit 1; }
  cmp -s "$FIXTURE" "$BOTTLE/drive_c/App/vfs_copy.txt" || { echo "FAIL: bottle copy mismatch"; exit 1; }
  [[ -f "$DRIVE_D/vfs_out.txt" ]] || { echo "FAIL: vfs_out.txt missing"; exit 1; }
  grep -Fq -- '---WIE_VFS---' "$DRIVE_D/vfs_out.txt" || { echo "FAIL: vfs_out.txt stamp missing"; exit 1; }
  grep -Fq -- 'Привет' "$DRIVE_D/vfs_out.txt" || { echo "FAIL: Russian missing"; exit 1; }
  grep -Fq -- '你好' "$DRIVE_D/vfs_out.txt" || { echo "FAIL: Chinese missing"; exit 1; }
  grep -Fq -- '日本語' "$DRIVE_D/vfs_out.txt" || { echo "FAIL: Japanese missing"; exit 1; }

  rm -rf "$BOTTLE" "$DRIVE_D"
fi

if [[ "$CATEGORY" == "all" || "$CATEGORY" == "cpp_exes" ]]; then
  run_one "$OUT/cpp_throw.exe"
  run_one "$OUT/cpp_dtor.exe"
  run_one "$OUT/cpp_multi_catch.exe"
  run_one "$OUT/cpp_types.exe"
  run_one "$OUT/cpp_threads.exe"
fi

if [[ "$CATEGORY" == "all" || "$CATEGORY" == "seh_exes" ]]; then
  run_one "$OUT/seh_access_violation.exe"
  run_one "$OUT/seh_div_zero.exe"
fi

if [[ "$CATEGORY" == "all" || "$CATEGORY" == "game" ]]; then
  printf '500\n250\n750\n' | run_one "$OUT/guess_price.exe" --stdin /dev/stdin
fi

if [[ "$CATEGORY" == "all" || "$CATEGORY" == "dll_tests" ]]; then
  for exe in "$OUT"/dll_*.exe; do
    [ -f "$exe" ] && run_one "$exe"
  done
fi

echo "=== micro-suite (${CATEGORY}): ok ==="
