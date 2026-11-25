#!/usr/bin/env bash
# Fast inner-loop gate: test what you touched, not the whole workspace.
#
# The problem this solves: `cargo nextest run -p wie-cpu` is ~4.5 s of test time
# but almost all of that is tests unrelated to the edit, and `-p wie-runtime`
# is ~120 s because it drives real guest PEs and a serialized GUI suite. Running
# those after every small change is the slow part, not the tests themselves.
#
# What it does, from the working-tree diff:
#   1. maps each changed path to a package (crates/<dir> → package name),
#   2. derives a test filter from the module path it touched,
#   3. runs `cargo check -p <pkg> --all-targets` (compile feedback, ~2 s warm),
#   4. runs only the matching tests for that package.
#
# Usage:
#   ./scripts/gate.sh --check          # compile only (the rename/refactor loop)
#   ./scripts/gate.sh                  # diff-driven: what you touched
#   ./scripts/gate.sh --list           # print the plan, run nothing
#   ./scripts/gate.sh --release        # also run the release profile (codegen changes)
#   ./scripts/gate.sh --full           # the pre-PR gate: whole touched packages
#   ./scripts/gate.sh --workspace      # everything, like CI (slow; use before pushing)
#   ./scripts/gate.sh --smoke          # build the CLI and run one micro guest
#   ./scripts/gate.sh --base origin/main   # diff against a branch instead of HEAD
#
# Measured on this repo (warm target dir, 2026-11-25):
#   cargo check -p wie-cpu --all-targets        ~1.4 s   → use for compile feedback
#   nextest -p wie-cpu (544 tests)              ~4.3 s   → cheap either way
#   nextest -p wie-cpu --release                +30-60 s → only for codegen changes
#   nextest -p wie-runtime (whole package)      ~120 s   → builds 52 GUI + 45 micro
#                                                          test binaries, each linked
#                                                          against the full runtime
#   nextest -p wie-runtime --lib <filter>       ~20-30 s → one lib test binary, 0.03 s
#                                                          of test time
# So: filtering saves nothing on wie-cpu, and `--lib` is the whole win on
# wie-runtime. The dominant cost is linking test binaries, not running tests —
# which is why `--check` exists.
#
# Exit status is the first failing step's, so it is usable in a loop or a hook.

set -uo pipefail

cd "$(dirname "$0")/.." || exit 1

mode="diff"          # diff | full | workspace
do_release=0
do_list=0
do_smoke=0
check_only=0
base=""
smoke_exe="micro-exes/out/crt_hello.exe"

while [ $# -gt 0 ]; do
    case "$1" in
        --full) mode="full" ;;
        --workspace) mode="workspace" ;;
        --release) do_release=1 ;;
        --list) do_list=1 ;;
        --check) check_only=1 ;;
        --smoke) do_smoke=1 ;;
        --base)
            shift
            base="${1:-}"
            [ -n "$base" ] || { echo "gate: --base needs a ref" >&2; exit 2; }
            ;;
        -h|--help)
            sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "gate: unknown argument: $1" >&2
            exit 2
            ;;
    esac
    shift
done

# ---------------------------------------------------------------------------
# Changed paths.
#
# Staged + unstaged + untracked, because an edit under review and an edit not yet
# staged are the same edit to the person running this. Against `--base`, compare
# the merge-base so a long-lived branch does not report every file as changed.
# ---------------------------------------------------------------------------
changed_paths() {
    if [ -n "$base" ]; then
        git diff --name-only "$base"...HEAD
    else
        { git diff --name-only HEAD
          git ls-files --others --exclude-standard
        } 2>/dev/null
    fi | sed '/^$/d' | sort -u
}

# Package name for a crate directory, read from its manifest (dir name and
# package name differ in this workspace, so guessing would be wrong).
package_for_dir() {
    local dir="$1" manifest="crates/$1/Cargo.toml"
    [ -f "$manifest" ] || return 1
    sed -n 's/^name[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' "$manifest" | head -1
}

# Test-name filter for a changed source file.
#
# nextest `test(<regex>)` matches the full test name, which starts with the module
# path (`mem::tests::a_peer_churn...`), so the module path is the filter. Three
# shapes matter:
#   crates/wie-cpu/src/mem/tests.rs      → mem        (tests live in mem::tests::)
#   crates/wie-cli/src/gui/app.rs        → gui::app
#   crates/wie-runtime/tests/micro_x.rs  → binary(micro_x)  (integration binary)
filter_for_path() {
    local path="$1"
    case "$path" in
        # `src` first: a `case` glob's `*` matches `/`, so `crates/*/tests/*.rs`
        # also matched `crates/wie-cpu/src/jit/tests/real_guest_blocks.rs` and
        # asked nextest for a test binary named after the module — `binary(...)`,
        # which no such target exists for. The narrower intent wins by order.
        crates/*/src/*.rs)
            # Strip the crate prefix, drop the file name, and treat a trailing
            # `tests`/`mod` segment as organisational rather than meaningful.
            local rel="${path#crates/*/src/}"
            rel="${rel%.rs}"
            rel="${rel%/mod}"
            rel="${rel%/tests}"
            if [ "$rel" = "$path" ] || [ -z "$rel" ] || [ "$rel" = "lib" ] || [ "$rel" = "main" ]; then
                printf ''
            else
                # Test names are `module::sub::case`, never `module/sub`.
                printf 'test(%s)' "${rel//\//::}"
            fi
            ;;
        crates/*/tests/*.rs)
            local bin
            bin="$(basename "$path" .rs)"
            printf 'binary(%s)' "$bin"
            ;;
        *) printf '' ;;
    esac
}

# Cargo target selector for a changed source file.
#
# This is the difference between 5 s and 55 s on `wie-runtime`: `-p <pkg>` builds
# **every** test target of the package — 52 GUI tests plus 45 micro-PE binaries,
# each linked against the full runtime — even when the only thing being run is one
# lib test. `--lib` / `--test <name>` narrows the build to what can hold the test.
target_for_path() {
    local path="$1"
    case "$path" in
        # Same ordering reason as `filter_for_path`: a module under `src/…/tests/`
        # is not a `--test` target, it is compiled into the lib target.
        crates/*/src/*.rs)   printf -- '--lib' ;;
        crates/*/tests/*.rs) printf -- '--test %s' "$(basename "$path" .rs)" ;;
        *) printf '' ;;
    esac
}

# Union of predicates: nextest uses `+`, and rejects `,`.
union() {
    local acc="" p
    for p in "$@"; do
        [ -n "$p" ] || continue
        if [ -z "$acc" ]; then acc="$p"; else acc="$acc + $p"; fi
    done
    printf '%s' "$acc"
}

run_step() {
    local label="$1"; shift
    echo "── $label"
    local t0=$SECONDS
    "$@"
    local rc=$?
    echo "   ($((SECONDS - t0))s, rc=$rc)"
    return $rc
}

if [ "$mode" = "workspace" ]; then
    echo "gate: workspace (CI shape — expect minutes, not seconds)"
    cargo clippy --workspace --all-targets || exit 1
    cargo fmt --all -- --check || exit 1
    cargo nextest run --workspace || exit 1
    [ "$do_release" -eq 1 ] && { cargo nextest run --workspace --release || exit 1; }
    echo "gate: workspace clean"
    exit 0
fi

paths="$(changed_paths)"
if [ -z "$paths" ]; then
    echo "gate: no changed files vs ${base:-HEAD} — nothing to do"
    exit 0
fi

# Group by package: one line per (package, filter) pair.
plan_dirs="$(printf '%s\n' "$paths" | sed -n 's#^crates/\([^/]*\)/.*#\1#p' | sort -u)"
plan_docs="$(printf '%s\n' "$paths" | grep -cv '^crates/' || true)"

if [ -z "$plan_dirs" ]; then
    echo "gate: only non-crate files changed ($plan_docs) — fmt only"
    [ "$do_list" -eq 1 ] && exit 0
    cargo fmt --all -- --check || exit 1
    echo "gate: clean (docs/scripts only; no crate touched)"
    exit 0
fi

packages=()
declare -A pkg_filters
declare -A pkg_targets
for dir in $plan_dirs; do
    pkg="$(package_for_dir "$dir")" || { echo "gate: no manifest for crates/$dir" >&2; exit 2; }
    packages+=("$pkg")
    filters=()
    targets=""
    while IFS= read -r path; do
        f="$(filter_for_path "$path")"
        [ -n "$f" ] && filters+=("$f")
        t="$(target_for_path "$path")"
        if [ -n "$t" ] && [ "${targets#*$t}" = "$targets" ]; then
            targets="${targets:+$targets }$t"
            # A lib filter and an integration filter cannot share one run: `--lib`
            # plus `--test x` is a union of targets, which nextest accepts, but the
            # filter then has to cover both. Keeping the union is correct; the
            # build is still narrower than the whole package.
        fi
    done < <(printf '%s\n' "$paths" | grep "^crates/$dir/")
    # shellcheck disable=SC2086
    pkg_targets["$pkg"]="$targets"
    # shellcheck disable=SC2086
    pkg_filters["$pkg"]="$(union "${filters[@]+"${filters[@]}"}")"
done

echo "gate: ${#packages[@]} package(s) touched: ${packages[*]}"
for pkg in "${packages[@]}"; do
    printf '      %-14s %-34s [%s]\n' "$pkg" \
        "${pkg_filters[$pkg]:-(whole package: no module filter derivable)}" \
        "${pkg_targets[$pkg]:-all targets}"
done

if [ "$do_list" -eq 1 ]; then
    exit 0
fi

for pkg in "${packages[@]}"; do
    run_step "check $pkg --all-targets" cargo check -p "$pkg" --all-targets || exit 1
    [ "$check_only" -eq 1 ] && continue

    if [ "$mode" = "full" ] || [ -z "${pkg_filters[$pkg]}" ]; then
        # shellcheck disable=SC2086
        run_step "nextest $pkg (full package)" cargo nextest run -p "$pkg" \
            ${pkg_targets[$pkg]} || exit 1
        [ "$do_release" -eq 1 ] && { run_step "nextest $pkg --release" cargo nextest run -p "$pkg" --release || exit 1; }
    else
        echo "── nextest $pkg -E '${pkg_filters[$pkg]}'"
        t0=$SECONDS
        # shellcheck disable=SC2086
        out="$(cargo nextest run -p "$pkg" ${pkg_targets[$pkg]} -E "${pkg_filters[$pkg]}" 2>&1)"
        rc=$?
        printf '%s\n' "$out" | tail -3
        # nextest exits 0 on "0 tests run", which is a green gate that exercises
        # nothing — the failure mode this repo has been bitten by before (a
        # fixture missing, 39 GUI tests "passing" in 0.02 s). Refuse it.
        if [ $rc -eq 0 ] && printf '%s' "$out" | grep -qE '[^0-9]0 tests run'; then
            echo "gate: FILTER MATCHED NO TESTS for $pkg (${pkg_filters[$pkg]})" >&2
            echo "gate: the filter is broken, not the code — rerun with --full" >&2
            exit 1
        fi
        echo "   ($((SECONDS - t0))s, rc=$rc)"
        [ $rc -eq 0 ] || { echo "gate: tests failed for $pkg" >&2; exit $rc; }
        [ "$do_release" -eq 1 ] && {
            run_step "nextest $pkg --release -E '${pkg_filters[$pkg]}'" \
                cargo nextest run -p "$pkg" --release -E "${pkg_filters[$pkg]}" || exit 1
        }
    fi
done

if [ "$do_smoke" -eq 1 ]; then
    run_step "build wie-cli" cargo build -p wie-cli || exit 1
    [ -f "$smoke_exe" ] || { echo "gate: no $smoke_exe (run: make -C micro-exes)" >&2; exit 1; }
    echo "── smoke: wie run $smoke_exe"
    t0=$SECONDS
    ./target/debug/wie run "$smoke_exe" < /dev/null > /dev/null 2>&1
    rc=$?
    echo "   ($((SECONDS - t0))s, rc=$rc)"
    [ $rc -eq 0 ] || { echo "gate: smoke failed (exit $rc)" >&2; exit 1; }
fi

echo "gate: clean"
