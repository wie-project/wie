#!/usr/bin/env bash
# Run micro-EXE test gates by category. Exit non-zero on first failure.
# Usage: ./run-micro-suite.sh [category] [--matrix] [--short]
#        ./run-micro-suite.sh exe <name> [-- args...]   # one guest only
#   category: all (default), cpp_exes, seh_exes, dll_tests, console_tests,
#             pthread_tests, exe
#   exe <name>: run a single guest from micro-exes/out (e.g. `exe long_loop`,
#             `exe mt_contention 4 512 ff` — trailing words are passed
#             straight to `wie run`). This is the same run_one() choke point
#             the categories use, so a single-exe run is exactly what the
#             category run would have done for that exe, minus staging
#             special-cases (dll_* still get --app-dir).
#   --matrix: additionally repeat the chosen category under alternate JIT
#             backends (WIE_JIT_MEM=slow, WIE_JIT_MEM=pin, WIE_CPU=iced).
#             Replaces the former scripts/check-jit-matrix.sh; use when
#             touching memory lowering, chaining, or CPU dispatch.
#   --short:  opt every fixture that supports it into its short work budget
#             by exporting WIE_SHORT=1 into the GUEST environment (via
#             WIE_GUEST_ENV). Fixtures that do not implement it ignore it,
#             so this is safe for a whole category; the pass/fail rule and
#             the assertions are unchanged. Env equivalent: WIE_SUITE_SHORT=1.
#
# The `all` category is DATA-DRIVEN: it runs every *.exe in micro-exes/out/
# applying per-exe overrides from the tables below. A new micro exe is
# therefore covered automatically — no list editing required.
#
# WIE_SKIP_LONG_LOOP=1 skips long_loop.exe (needed under WIE_CPU=iced, where
# its 100M-iteration loop exceeds the slice budget). Note `--short` achieves
# the same wall-time relief without dropping the exe from the sweep.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CLI="${CLI:-$ROOT/target/release/wie}"
CPU="${WIE_CPU:-jit}"

usage() { sed -n '2,29p' "$0"; }

CATEGORY="all"
MATRIX=0
SHORT=0
POS=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --matrix) MATRIX=1 ;;
    --short) SHORT=1 ;;
    -h|--help) usage; exit 0 ;;
    -*) echo "run-micro-suite: unknown option: $1" >&2; usage >&2; exit 2 ;;
    *) POS+=("$1") ;;
  esac
  shift
done

CATEGORY="${POS[0]:-all}"
ONLY_EXE=""
EXE_ARGS=()
if [[ "$CATEGORY" == "exe" ]]; then
  ONLY_EXE="${POS[1]:-}"
  if [[ -z "$ONLY_EXE" ]]; then
    echo "run-micro-suite: 'exe' needs a guest name, e.g. 'exe long_loop'" >&2
    echo "available:" >&2
    ls -1 "$ROOT/micro-exes/out"/*.exe 2>/dev/null | sed 's|.*/|  |; s|\.exe$||' >&2 || true
    exit 2
  fi
  ONLY_EXE="${ONLY_EXE%.exe}"
  EXE_ARGS=()
  if [[ "${#POS[@]}" -gt 2 ]]; then
    EXE_ARGS=("${POS[@]:2}")
  fi
elif [[ "${#POS[@]}" -gt 1 ]]; then
  echo "run-micro-suite: unexpected extra arguments: ${POS[*]:1}" >&2
  usage >&2
  exit 2
fi

# Short mode is a GUEST-environment switch, so it travels through
# WIE_GUEST_ENV (the runtime's injection hook) rather than the host env —
# the guest env does not inherit the host's. Exported once, before run_suite,
# so the --matrix re-invocations cannot append it twice.
if [[ "$SHORT" == 1 || -n "${WIE_SUITE_SHORT:-}" ]]; then
  case "${WIE_GUEST_ENV:-}" in
    *WIE_SHORT=1*) : ;;
    "") export WIE_GUEST_ENV="WIE_SHORT=1" ;;
    *) export WIE_GUEST_ENV="WIE_SHORT=1;${WIE_GUEST_ENV}" ;;
  esac
fi

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

  # Build only what the chosen category runs. Most categories have a phony
  # Makefile target of the same name; the two that do not map to their exe
  # list here. Without this mapping `run-micro-suite.sh console_tests` died in
  # make with "No rule to make target `console_tests'" before running a single
  # guest, even though the category is documented in the usage header.
  # (seh_exes is deliberately NOT mapped: micro-exes/seh/ holds only
  # cpp_exceptions/, so neither seh_access_violation.exe nor seh_div_zero.exe
  # has a source or a build rule. The category is dead in the Makefile and
  # fails here exactly as it did before; reviving it needs new fixtures.)
  local -a build_targets
  case "$CATEGORY" in
    console_tests) build_targets=("out/console_cells.exe") ;;
    pthread_tests) build_targets=("out/pt_basic.exe" "out/pt_cond.exe") ;;
    exe)
      # The file rule if the Makefile has one, else the phony alias
      # (e.g. `exe gl_quad` -> out/gl_quad.exe, `exe dir_watch` -> phony).
      if ! make -C "$ROOT/micro-exes" "out/${ONLY_EXE}.exe" 2>/dev/null \
        && ! make -C "$ROOT/micro-exes" "${ONLY_EXE}" 2>/dev/null; then
        echo "run-micro-suite: no Makefile rule builds '${ONLY_EXE}' (tried" \
          "out/${ONLY_EXE}.exe and ${ONLY_EXE})" >&2
        exit 1
      fi
      build_targets=()
      ;;
    *) build_targets=("$CATEGORY") ;;
  esac
  if [[ "${#build_targets[@]}" -gt 0 ]]; then
    make -C "$ROOT/micro-exes" "${build_targets[@]}"
  fi

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

  # Single-exe selector: the same run_one() choke point the categories use.
  # An explicitly named exe is never filtered by SKIP_EXES (asking for one is
  # the point) but a note is printed so a known-unrunnable guest is not a
  # surprise.
  if [[ "$CATEGORY" == "exe" ]]; then
    if skipped "${ONLY_EXE}.exe"; then
      echo "--- note: ${ONLY_EXE}.exe is in SKIP_EXES; running it anyway on explicit request ---"
    fi
    local target="$OUT/${ONLY_EXE}.exe"
    if [[ ! -f "$target" ]]; then
      echo "run-micro-suite: no such guest: $target" >&2
      exit 1
    fi
    # Args: explicit trailing words win, else the sweep's EXTRA_ARGS entry for
    # this exe (so `exe cli_args` behaves like the `all` sweep's cli_args run).
    local -a args=()
    if [[ "${#EXE_ARGS[@]}" -gt 0 ]]; then
      args=("${EXE_ARGS[@]}")
    else
      local from_table
      from_table="$(extra_args_for "${ONLY_EXE}.exe")"
      [[ -n "$from_table" ]] && read -r -a args <<<"$from_table"
    fi
    # Same --app-dir staging the `all` sweep gives dll_* (they load their
    # sibling *_funcs.dll by bare name).
    if [[ "${ONLY_EXE}" == dll_* ]]; then
      run_one "$target" --app-dir "$OUT" ${args[@]+"${args[@]}"}
    elif [[ "${#args[@]}" -gt 0 ]]; then
      run_one "$target" "${args[@]}"
    else
      run_one "$target"
    fi
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
