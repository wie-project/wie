#!/usr/bin/env bash
# Run micro-EXE test gates by category. Exit non-zero on first failure.
# Usage: ./run-micro-suite.sh [category]
#   category: all (default), cpp_exes, seh_exes, dll_tests, console_tests,
#             pthread_tests
#
# The `all` category is DATA-DRIVEN: it runs every *.exe in micro-exes/out/
# (79 today), applying per-exe overrides from the tables below. A new micro
# exe is therefore covered automatically — no list editing required.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CLI="${CLI:-$ROOT/target/release/wie}"
CPU="${WIE_CPU:-jit}"
CATEGORY="${1:-all}"

# The per-run summary (path, events, exit) is a `wie::commands::run`-target
# debug log; show it by default so the suite reports what each exe did.
# (Note: EnvFilter matches string prefixes, so `wie` alone would also enable
# `wie_cpu`/`wiegui`/`wie_runtime` — the full target path keeps the noise out.)
export RUST_LOG="${RUST_LOG:-wie::commands::run=debug}"

if [[ ! -x "$CLI" ]]; then
  echo "building wie (release)…"
  cargo build -p wie-cli --release --manifest-path "$ROOT/Cargo.toml"
fi

export WIE_CPU="$CPU"
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
