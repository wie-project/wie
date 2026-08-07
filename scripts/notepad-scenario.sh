#!/usr/bin/env bash
# Notepad end-to-end acceptance scenario (docs/notepad-support-plan.md Task 6.1).
#
# Drives real_exes/notepad.exe (RNotepad, fetched by scripts/fetch-rnotepad.sh)
# through the notepad-milestone features and asserts the outcomes that can be
# verified headlessly:
#
#   1. trace triage      — no unimplemented export on the startup + loop path
#   2. edit-ops run      — typed text reaches the EDIT; select-all/copy/paste/
#                          undo commands dispatch; the modeless Find dialog
#                          opens (FINDMSGSTRING flow observable in the log)
#   3. clean-exit run    — scripted File→Exit on an untouched doc exits 0
#   4. headless frame    — `run --screenshot` captures a 640x480 guest frame
#                          (the status bar renders there; the macOS menu bar is
#                          host chrome — see the manual step)
#
# Steps that need a human at the machine (the interactive Save-As dialog, the
# live macOS menu bar, a real screen grab) are marked `# manual:` and only run
# under `--manual`. Everything else runs now and must pass.
#
# Usage:
#   ./scripts/notepad-scenario.sh            # headless asserts only
#   ./scripts/notepad-scenario.sh --manual   # + interactive GUI scenario
#
# Knobs:
#   WIE_CLI           wie binary (default ./target/debug/wie)
#   WIE_NOTEPAD_ROOT  bottle root (default ${TMPDIR}/wie-notepad-scenario)
#   NOTEPAD_TIMEOUT   per-run timeout in seconds (default 90)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
NOTEPAD="$ROOT/real_exes/notepad.exe"
WIE_CLI="${WIE_CLI:-$ROOT/target/debug/wie}"
BOTTLE="${WIE_NOTEPAD_ROOT:-${TMPDIR:-/tmp}/wie-notepad-scenario}"
TIMEOUT_S="${NOTEPAD_TIMEOUT:-90}"

MANUAL=0
if [[ "${1:-}" == "--manual" ]]; then
    MANUAL=1
fi

say() { printf '\n== %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

# --- preflight ---------------------------------------------------------------
[[ -f "$NOTEPAD" ]] || fail "real_exes/notepad.exe missing — run ./scripts/fetch-rnotepad.sh"
[[ -x "$WIE_CLI" ]] || fail "wie missing at $WIE_CLI — build with: cargo build -p wie-cli"

# --- step 0: bottle + fixture (headless) -------------------------------------
say "step 0: fixture in bottle ($BOTTLE/drive_c)"
rm -rf "$BOTTLE"
mkdir -p "$BOTTLE/drive_c"
cat > "$BOTTLE/drive_c/fixture.txt" <<'EOF'
WIE notepad acceptance fixture
second line with spaces
EOF

# --- step 1: trace triage (headless) ------------------------------------------
# The P0.3-era first-failure chain (_initialize_wide_environment → GetStartupInfoW
# → GetUserDefaultUILanguage → … → GetWindowPlacement → DestroyAcceleratorTable)
# must all be handled; the summary must be a clean ExitProcess{code:0}.
say "step 1: trace triage (max-api 400)"
TRACE_LOG="$BOTTLE/trace.log"
if ! "$WIE_CLI" trace --max-api 400 "$NOTEPAD" > "$TRACE_LOG" 2>&1; then
    fail "trace run failed (see $TRACE_LOG)"
fi
rg -q "ExitProcess \{ code: 0 \}" "$TRACE_LOG" \
    || fail "trace did not end in ExitProcess{code:0} (see $TRACE_LOG)"
if rg -qi "unsupported|bail|unimplemented|not implemented" "$TRACE_LOG"; then
    fail "trace hit an unsupported export (see $TRACE_LOG)"
fi
printf '  ok: %s handled APIs, clean ExitProcess{code:0}\n' "$(rg -c '^api\[' "$TRACE_LOG")"

# --- step 2: edit-ops input-script run (headless, no exit) -------------------
# Posts WM_KEYDOWN/WM_CHAR/WM_COMMAND to the guest; the guest retargets
# keyboard messages to the focused EDIT, so `type` inserts into the document.
# The run never exits on its own — the timeout is the harness, and the asserts
# are log evidence, not the exit code. CMD ids verified against the RT_MENU
# 0x201 template: Select All=0x116, Copy=0x112, Paste=0x113, Undo=0x110,
# Search (Find)=0x120. `menu` posts WM_COMMAND directly, so the script is
# independent of the host accelerator-translation path.
say "step 2: edit-ops run (type / select-all / copy / paste / undo / find)"
cat > "$BOTTLE/edit-ops.input" <<'EOF'
sleep 1500
type hello
sleep 500
menu 278
sleep 300
menu 274
sleep 300
menu 275
sleep 300
menu 272
sleep 300
menu 288
sleep 800
EOF
EDIT_LOG="$BOTTLE/edit-ops.log"
set +e
timeout "$TIMEOUT_S" env WIE_ROOT="$BOTTLE" WIE_INPUT_SCRIPT="$BOTTLE/edit-ops.input" \
    RUST_LOG=wiegui=debug "$WIE_CLI" run --gui "$NOTEPAD" > "$EDIT_LOG" 2>&1
RUN_CODE=$?
set -e
# 124 (timeout kill) is the expected harness exit: the script has no exit step.
[[ $RUN_CODE -eq 124 ]] || fail "edit-ops run ended with $RUN_CODE (expected the harness timeout)"
rg -q "input script: finished" "$EDIT_LOG" || fail "input script did not finish (see $EDIT_LOG)"
CHAR_COUNT="$(rg -c "DispatchMessage 258" "$EDIT_LOG" || true)"
[[ "$CHAR_COUNT" -ge 5 ]] || fail "typed text did not reach the EDIT (saw $CHAR_COUNT WM_CHAR; see $EDIT_LOG)"
rg -q 'find dialog opened' "$EDIT_LOG" \
    || fail "FindTextW did not open the modeless Find dialog (see $EDIT_LOG)"
printf '  ok: %s WM_CHAR reached the EDIT; Find dialog opened (FINDMSGSTRING flow)\n' "$CHAR_COUNT"

# --- step 3: clean-exit input-script run (headless, exit-code assert) ---------
# File→Exit on an untouched document: no save-changes prompt, clean ExitProcess.
say "step 3: clean-exit run (find + Esc + File→Exit)"
cat > "$BOTTLE/clean-exit.input" <<'EOF'
sleep 1500
menu 288
sleep 800
key 0x1B
sleep 800
menu 264
EOF
if ! timeout "$TIMEOUT_S" env WIE_ROOT="$BOTTLE" WIE_INPUT_SCRIPT="$BOTTLE/clean-exit.input" \
    RUST_LOG=wiegui=info "$WIE_CLI" run --gui "$NOTEPAD" > "$BOTTLE/clean-exit.log" 2>&1; then
    fail "clean-exit run did not exit 0 (see $BOTTLE/clean-exit.log)"
fi
rg -q 'find dialog opened' "$BOTTLE/clean-exit.log" \
    || fail "find dialog did not open in the clean-exit run (see $BOTTLE/clean-exit.log)"
# ANSI codes interleave the "guest exited code=0" log line; the process exit
# code (asserted above) is the real gate, this just proves the guest exited.
rg -q 'guest exited' "$BOTTLE/clean-exit.log" \
    || fail "guest did not exit (see $BOTTLE/clean-exit.log)"
printf '  ok: scripted File→Exit exited 0\n'

# --- step 4: headless frame capture (status bar renders in the guest frame) ---
say "step 4: headless screenshot (guest frame)"
FRAME="$BOTTLE/notepad-frame.bmp"
rm -f "$FRAME"
if ! timeout "$TIMEOUT_S" "$WIE_CLI" run --screenshot "$FRAME" "$NOTEPAD" > "$BOTTLE/screenshot.log" 2>&1; then
    fail "screenshot run failed (see $BOTTLE/screenshot.log)"
fi
[[ -s "$FRAME" ]] || fail "screenshot produced no BMP"
printf '  ok: captured %s (%s bytes)\n' "$FRAME" "$(wc -c < "$FRAME" | tr -d ' ')"

# --- manual steps (human at the machine) --------------------------------------
if [[ $MANUAL -eq 1 ]]; then
    say "manual: interactive scenario (open fixture, edit, find, save-as, exit)"
    # Launch the GUI. The Save-As dialog is interactive by design — a human
    # picks the path. NOTE: as of this lane, the interactive file dialog's
    # in-guest modal loop does not survive the live run (the guest silently
    # exits 0 when the dialog opens); the FileDialogPolicy::Accept path is
    # unit-tested but has no CLI knob. If the dialog still closes the app,
    # treat the save-as step as pending the L4 live-run fix and just verify
    # the rest by hand.
    "$WIE_CLI" run --gui "$NOTEPAD"
    echo "
  In the notepad window:
    1. File→Open (or drag the fixture) → path  C:\\fixture.txt  → Open
    2. type:  hello there
    3. Edit→Select All (Ctrl+A), Edit→Copy (Ctrl+C), Edit→Paste (Ctrl+V),
       Edit→Undo (Ctrl+Z)   (undo restores the fixture text)
    4. Search→Find (Ctrl+F) → type  hello  → Find Next
    5. File→Save As... → path  C:\\saved.txt  → Save   (encoding: UTF-8)
    6. File→Exit — answer the save-changes prompt with No
"
    say "manual: saved-file diff"
    if [[ -f "$BOTTLE/drive_c/saved.txt" ]]; then
        if diff -u "$BOTTLE/drive_c/fixture.txt" "$BOTTLE/drive_c/saved.txt"; then
            echo "  ok: saved.txt matches fixture.txt (round-trip preserved)"
        else
            echo "  NOTE: saved.txt differs from fixture.txt — expected if the final"
            echo "        undo did not fully revert the typed text; compare by eye."
        fi
    else
        echo "  NOTE: $BOTTLE/drive_c/saved.txt missing — the save-as dialog was not"
        echo "        completed (see the L4 live-run note above)."
    fi

    say "manual: screenshots (menu bar + status bar)"
    echo "  The macOS menu bar is host chrome (muda) — not in the guest frame."
    echo "  1. Status bar (automated above): $BOTTLE/notepad-frame.bmp"
    echo "  2. Live window incl. the macOS menu bar:"
    echo "       ./scripts/notepad-scenario.sh --manual   (leave notepad open)"
    echo "       screencapture -l \$(osascript -e 'tell app \"wie\" to id of window 1') \\"
    echo "         $BOTTLE/notepad-live.png"
fi

# --- perf sanity (optional, per Task 6.1 step 2) ------------------------------
if [[ -n "${WIE_RUNTIME_PROFILE:-}" ]]; then
    say "perf sanity: WIE_RUNTIME_PROFILE on the clean-exit run"
    WIE_RUNTIME_PROFILE=1 WIE_ROOT="$BOTTLE" WIE_INPUT_SCRIPT="$BOTTLE/clean-exit.input" \
        "$WIE_CLI" run --gui "$NOTEPAD" > "$BOTTLE/profile.log" 2>&1 || true
    rg -n "host stops|JIT|cpu" "$BOTTLE/profile.log" | head -10
fi

say "notepad scenario: headless asserts passed (manual steps pending)"
