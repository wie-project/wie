#!/usr/bin/env bash
# Run micro-EXE test gates by category. Exit non-zero on first failure.
# Usage: ./run-micro-suite.sh [category] [--matrix]
#   category: all (default), cpp_exes, seh_exes, dll_tests, console_tests,
#             pthread_tests
#   --matrix: additionally repeat the chosen category under alternate JIT
#             backends (WIE_JIT_MEM=slow, WIE_JIT_MEM=pin, WIE_CPU=iced).
#             Replaces the former scripts/check-jit-matrix.sh; use when
#             touching memory lowering, chaining, or CPU dispatch.
#
# The `all` category is DATA-DRIVEN: it runs every *.exe in micro-exes/out/
# applying per-exe overrides from the tables below. A new micro exe is
# therefore covered automatically — no list editing required.
#
# WIE_SKIP_LONG_LOOP=1 skips long_loop.exe (needed under WIE_CPU=iced, where
# its 100M-iteration loop exceeds the slice budget).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CLI="${CLI:-$ROOT/target/release/wie}"
CPU="${WIE_CPU:-jit}"

CATEGORY="all"
MATRIX=0
for arg in "$@"; do
  case "$arg" in
    --matrix) MATRIX=1 ;;
    -h|--help)
      sed -n '2,12p' "$0"; exit 0 ;;
    *) CATEGORY="$arg" ;;
  esac
done

# The per-run summary (path, events, exit) is a `wie::commands::run`-target
# debug log; show it by default so the suite reports what each exe did.
# (Note: EnvFilter matches string prefixes, so `wie` alone would also enable
# `wie_cpu`/`wiegui`/`wie_runtime` — the full target path keeps the noise out.)
export RUST_LOG="${RUST_LOG:-wie::commands::run=debug}"

if [[ ! -x "$CLI" ]]; then
  echo "building wie (release)…"
  cargo build -p wie-cli --release --manifest-path "$ROOT/Cargo.toml"
fi

run_suite() {
  export WIE_CPU="${WIE_SUITE_CPU:-$CPU}"
  echo "=== micro-suite: ${CATEGORY} (WIE_CPU=$WIE_CPU) ==="

  run_one() {
    local pe="$1"
    shift
    echo "--- run ${pe##*/} $* ---"
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

  # ---------------------------------------------------------------------------
  # Per-exe overrides for the data-driven `all` sweep.
  #
  # SKIP_EXES   — not runnable standalone; each entry documents why and where
  #               the behaviour is actually gated instead.
  # EXTRA_ARGS  — additional CLI args an exe needs under the suite.
  # ---------------------------------------------------------------------------
  SKIP_EXES=(
    # Interactive: block on stdin waiting for keys; nothing to assert headless.
    "snake.exe"
    "guess_price.exe"
    # GUI pump demos: persistent message loops, exercised with full window
    # semantics by the wie-runtime::micro_gui_window integration tests instead
    # (a CLI run here would just hit the slice watchdog).
    "gui_blit.exe" "gui_control.exe" "gui_d3d9.exe" "gui_demo.exe"
    "gui_dialog.exe" "gui_edit.exe" "gui_menu.exe" "gui_text.exe"
    "notepad_smoke.exe"
    # Need outbound network / a live HTTP peer; environment-dependent.
    "http_get.exe" "wininet_http.exe"
    # Spawn CHILD half of the CreateProcess pair — validated indirectly by
    # spawn_child.exe, which stages and supervises it inside a bottle.
    "child_proc.exe"
  )
  if [[ -n "${WIE_SKIP_LONG_LOOP:-}" ]]; then
    SKIP_EXES+=("long_loop.exe")
  fi

  EXTRA_ARGS=(
    "cli_args.exe:-n 3 -m hi"
    "mbwc_lasterror.exe:"
  )

  extra_args_for() {
    local name="$1" entry
    for entry in "${EXTRA_ARGS[@]:-}"; do
      [[ -z "$entry" ]] && continue
      if [[ "${entry%%:*}" == "$name" ]]; then
        echo "${entry#*:}"
        return
      fi
    done
  }

  skipped() {
    local name="$1" entry
    for entry in "${SKIP_EXES[@]:-}"; do
      [[ "$entry" == "$name" ]] && return 0
    done
    return 1
  }

  run_all_exes() {
    local exe name args
    shopt -s nullglob
    for exe in "$OUT"/*.exe; do
      name="${exe##*/}"
      if skipped "$name"; then
        echo "--- skip $name (see SKIP_EXES) ---"
        continue
      fi
      case "$name" in
        spawn_child.exe)
          # CreateProcess pair: stage child + parent in one bottle (the child
          # embeds the compile-time path C:\child_proc.exe); parent waits and
          # checks exit 42.
          SPAWN_ROOT="$(mktemp -d)"
          mkdir -p "$SPAWN_ROOT/drive_c"
          cp "$OUT/child_proc.exe" "$SPAWN_ROOT/drive_c/child_proc.exe"
          run_bottle_n2 "$exe" "$SPAWN_ROOT"
          rm -rf "$SPAWN_ROOT"
          ;;
        modules.exe)
          # Requires guest identity C:\App\modules.exe: stage into a temp
          # bottle and run the IN-BOTTLE copy so host_path_to_guest remaps the
          # module path through drive_c (plain `run` keeps C:\modules.exe →
          # guest exit 3).
          MODULES_ROOT="$(mktemp -d)"
          mkdir -p "$MODULES_ROOT/drive_c/App"
          cp "$OUT/modules.exe" "$MODULES_ROOT/drive_c/App/"
          run_bottle_n2 "$MODULES_ROOT/drive_c/App/modules.exe" "$MODULES_ROOT"
          rm -rf "$MODULES_ROOT"
          ;;
        msvcrt_gaps.exe)
          # fgetwc/getc consume injected console stdin ("AB"); without it the
          # first fgetwc hits EOF → guest exit 1. The full behavioural gate
          # lives in crates/wie-runtime/tests/micro_msvcrt_gaps.rs.
          STDIN_FIX="$(mktemp)"
          printf 'AB' >"$STDIN_FIX"
          run_one "$exe" --stdin "$STDIN_FIX"
          rm -f "$STDIN_FIX"
          ;;
        read_file.exe)
          # Reads C:\App\n2_in.txt ("hello-n2", OPEN_EXISTING): stage the
          # fixture into a temp bottle (original N2 contract).
          N2IN_ROOT="$(mktemp -d)"
          mkdir -p "$N2IN_ROOT/drive_c/App"
          printf 'hello-n2' >"$N2IN_ROOT/drive_c/App/n2_in.txt"
          run_bottle_n2 "$exe" "$N2IN_ROOT"
          rm -rf "$N2IN_ROOT"
          ;;
        vfs_roundtrip.exe)
          # D:↔C: UTF-8 round-trip: needs a bottle (copy target) plus a
          # --drive-d bridge with the staged fixture; verify both outputs on
          # the host side (byte-identical copy; stamped, Unicode output).
          VFS_BOTTLE="$(mktemp -d)"
          VFS_DRIVE_D="$(mktemp -d)"
          mkdir -p "$VFS_BOTTLE/drive_c/App"
          FIXTURE="$ROOT/micro-exes/vfs_roundtrip/fixture_utf8.txt"
          cp "$FIXTURE" "$VFS_DRIVE_D/vfs_in.txt"
          run_bottle_n2 "$exe" "$VFS_BOTTLE" --drive-d "$VFS_DRIVE_D"
          if [[ ! -f "$VFS_BOTTLE/drive_c/App/vfs_copy.txt" ]]; then
            echo "FAIL: vfs_copy.txt not created in bottle" >&2
            exit 1
          fi
          if ! cmp -s "$FIXTURE" "$VFS_BOTTLE/drive_c/App/vfs_copy.txt"; then
            echo "FAIL: bottle copy is not byte-identical to fixture" >&2
            exit 1
          fi
          if [[ ! -f "$VFS_DRIVE_D/vfs_out.txt" ]]; then
            echo "FAIL: vfs_out.txt not written back to host D:" >&2
            exit 1
          fi
          if ! grep -q -e '---WIE_VFS---' "$VFS_DRIVE_D/vfs_out.txt"; then
            echo "FAIL: vfs_out.txt missing stamp" >&2
            exit 1
          fi
          if ! grep -q 'Привет' "$VFS_DRIVE_D/vfs_out.txt"; then
            echo "FAIL: Russian missing in host output" >&2
            exit 1
          fi
          if ! grep -q '你好' "$VFS_DRIVE_D/vfs_out.txt"; then
            echo "FAIL: Chinese missing in host output" >&2
            exit 1
          fi
          rm -rf "$VFS_BOTTLE" "$VFS_DRIVE_D"
          ;;
        write_file.exe)
          # Writes C:\App\n2_out.txt through the VFS; verify the host-side
          # file actually landed with the expected content (guest exit 0 alone
          # would not prove the write left the emulator).
          N2OUT_ROOT="$(mktemp -d)"
          mkdir -p "$N2OUT_ROOT/drive_c/App"
          run_bottle_n2 "$exe" "$N2OUT_ROOT"
          if [[ ! -f "$N2OUT_ROOT/drive_c/App/n2_out.txt" ]]; then
            echo "FAIL: n2_out.txt not created on host" >&2
            exit 1
          fi
          if ! grep -q 'WIE_N2' "$N2OUT_ROOT/drive_c/App/n2_out.txt"; then
            echo "FAIL: n2_out.txt content mismatch" >&2
            exit 1
          fi
          rm -rf "$N2OUT_ROOT"
          ;;
        dll_*.exe)
          # DLL tests load their sibling *_funcs.dll files by bare name, so the
          # whole out/ folder is named explicitly with --app-dir (the default
          # `wie run` staging copies only the exe itself).
          run_one "$exe" --app-dir "$OUT"
          ;;
        *)
          args="$(extra_args_for "$name")"
          if [[ -n "$args" ]]; then
            run_one "$exe" $args
          else
            run_one "$exe"
          fi
          ;;
      esac
    done
    shopt -u nullglob
  }

  if [[ "$CATEGORY" == "all" ]]; then
    run_all_exes
  fi

  if [[ "$CATEGORY" == "console_tests" ]]; then
    run_one "$OUT/console_cells.exe"
  fi

  if [[ "$CATEGORY" == "pthread_tests" ]]; then
    run_one "$OUT/pt_basic.exe"
    run_one "$OUT/pt_cond.exe"
  fi

  if [[ "$CATEGORY" == "cpp_exes" ]]; then
    run_one "$OUT/cpp_throw.exe"
    run_one "$OUT/cpp_dtor.exe"
    run_one "$OUT/cpp_multi_catch.exe"
    run_one "$OUT/cpp_types.exe"
    run_one "$OUT/cpp_threads.exe"
  fi

  if [[ "$CATEGORY" == "seh_exes" ]]; then
    run_one "$OUT/seh_access_violation.exe"
    run_one "$OUT/seh_div_zero.exe"
  fi

  if [[ "$CATEGORY" == "dll_tests" ]]; then
    # Same --app-dir staging as the `all` sweep.
    shopt -s nullglob
    for exe in "$OUT"/dll_*.exe; do
      [ -f "$exe" ] && run_one "$exe" --app-dir "$OUT"
    done
    shopt -u nullglob
  fi

  echo "=== micro-suite (${CATEGORY}): ok ==="
}

if [[ "$MATRIX" == 1 ]]; then
  WIE_SUITE_CPU=jit WIE_JIT_MEM=slow run_suite
  WIE_SUITE_CPU=jit WIE_JIT_MEM=pin run_suite
  WIE_SKIP_LONG_LOOP=1 WIE_SUITE_CPU=iced run_suite
  echo "=== JIT matrix: all backends passed ==="
else
  run_suite
fi
