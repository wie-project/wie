# Wave 2 Acceptance Gate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Make `scripts/acceptance-wave2.sh` prove that the production `wie run --gui` path executes the D3D9 capture render thread and keeps conservative WinAPI handler time at or below 1 ms per accepted guest Present.

**Architecture:** Count accepted D3D9 `Present` handler entries in `PresentChannel` before selecting capture, commit, or legacy output. Copy that lock-free count into `RuntimeProfile` and emit it alongside legacy, capture, commit, and host-present counters. The acceptance script runs the native GUI adapter against an isolated temporary bottle, clears render-path opt-outs, accepts only watchdog termination statuses, evaluates hard invariants, and writes a baseline only after every invariant passes.

**Tech Stack:** Rust, Bash, Python 3, `cargo nextest`, winit/wgpu, D3D9 software renderer, macOS logged-in GUI session.

**Spec:** `docs/implementation-plan.md:32-39`, `docs/architecture-review-games-and-apps.md:173-175`, and `docs/handoff-wave2-next-steps.md:67-80`.

## Global Constraints

- Do not change general headless behavior or enable capture in headless/CI runs.
- Do not change `micro-exes/gui_d3d9/main.c`.
- Do not add a benchmark guest, profiling redesign, SSE work, or a new GUI mode.
- Keep process CPU informational and never describe it as guest-thread CPU.
- Keep the existing capture and commit frame-hash tests as correctness oracles. (Superseded 2026-09-25: the commit render thread and its hash test were removed; `gui_d3d9_capture_stream_frame_matches_legacy_hash` is the surviving oracle against the legacy in-handler raster path.)
- Run native acceptance only from a logged-in macOS GUI session.
- Use release binaries for the acceptance measurement.
- Do not update acceptance status or claim success before the real run passes.
- Preserve the current Wave 3–6 reconciliation and historical handoff notes.
- Follow workspace lints: no new `unwrap`, `expect`, `panic`, indexing, casts, or unsafe code.
- Do not commit automatically; leave the integrated diff for review.

---

### Task 1: Count D3D9 Present Entries Before Output Selection

**Files:**
- Modify: `crates/wie-winapi/src/present/mod.rs:212-269,348-432`
- Modify: `crates/wie-winapi/src/d3d9/device.rs:330-403`
- Test: `crates/wie-winapi/src/state/tests/d3d9_tests.rs:405-448`

**Interfaces:**
- Consumes: the D3D9 `Present` handler boundary and `PresentChannel` lock-free counters.
- Produces: `PresentChannel::record_d3d9_present(&self)` and `PresentChannel::present_enqueued(&self) -> u64`.

**Validation owner:** WinAPI/present owner.

- [x] **Step 1: Add the failing handler-boundary test**

Add after `test_d3d9_present_publishes_surface_frame`:

```rust
#[test]
fn test_d3d9_present_counts_entry_before_output_branch() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    {
        let d3d = state.d3d9();
        d3d.d3d9_backbuffer_width = 4;
        d3d.d3d9_backbuffer_height = 3;
        d3d.d3d9_backbuffer = vec![0_u32; 12];
        d3d.d3d9_present_hwnd = crate::handles::Hwnd::from(0x7777);
    }
    state.window_state().window_width = 4;
    state.window_state().window_height = 3;

    assert_eq!(state.present().channel.present_enqueued(), 0);

    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_present(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    assert_eq!(state.present().channel.present_enqueued(), 1);

    state.d3d9().d3d9_backbuffer.clear();
    write_regs(&mut engine, 1, 0, 0, 0, 0);
    assert_return_value!(
        d3d9::handle_present(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );

    assert_eq!(
        state.present().channel.present_enqueued(),
        2,
        "Present entries are counted before the output-eligibility branch"
    );
}
```

- [x] **Step 2: Verify the test fails**

Run:

```bash
cargo nextest run -p wie-winapi \
  -E 'test(test_d3d9_present_counts_entry_before_output_branch)'
```

Expected before implementation: compilation fails because `PresentChannel` has no `present_enqueued` method.

- [x] **Step 3: Add the counter to `PresentChannel`**

Add the field beside `published_seq` and `taken_seq`:

```rust
present_enqueued: AtomicU64,
```

Initialize it in `PresentChannel::new`:

```rust
present_enqueued: AtomicU64::new(0),
```

Add the methods:

```rust
/// Record one accepted D3D9 Present handler entry before output-path selection.
pub(crate) fn record_d3d9_present(&self) {
    self.present_enqueued.fetch_add(1, Ordering::Relaxed);
}

/// Number of accepted D3D9 Present handler entries.
#[must_use]
pub fn present_enqueued(&self) -> u64 {
    self.present_enqueued.load(Ordering::Relaxed)
}
```

- [x] **Step 4: Record at the handler boundary**

In `handle_present`, insert the increment immediately after the accepted `this` argument is read and before pacing or output selection:

```rust
let _this_pointer = read_arg(engine, ArgReg::Rcx, "IDirect3DDevice9::Present")?;
state.present().channel.record_d3d9_present();

present_pacing_wait(state);
```

Do not move the increment into `flush_present`, commit enqueue, or legacy publish; those sites lose calls through latest-wins coalescing or output eligibility.

- [x] **Step 5: Verify the focused and neighboring tests**

```bash
cargo nextest run -p wie-winapi \
  -E 'test(test_d3d9_present_counts_entry_before_output_branch) or test(test_d3d9_present_publishes_surface_frame)'
```

Expected: both tests pass.

---

### Task 2: Carry `present_enqueued` Through Runtime Profiling

**Files:**
- Modify and test: `crates/wie-runtime/src/session/profile.rs:10-66,155-231,333-505,576-767`

**Interfaces:**
- Consumes: `PresentChannel::present_enqueued(&self) -> u64` from Task 1.
- Produces: `RuntimeProfile::present_enqueued(&self) -> u64` and a capture-only profile line containing every present-path counter.

**Validation owner:** Runtime/profile owner.

- [x] **Step 1: Add the profile field and getter**

Add beside `present_ns`:

```rust
present_enqueued: u64,
```

Add:

```rust
/// Number of accepted D3D9 Present handler entries.
#[must_use]
pub fn present_enqueued(&self) -> u64 {
    self.present_enqueued
}
```

Do not change report formatting yet.

- [x] **Step 2: Add the failing capture-only formatting test**

```rust
#[test]
fn report_emits_all_present_path_counters_for_capture_only_session() {
    let profile = RuntimeProfile {
        present_enqueued: 9,
        capture_frames: 7,
        capture_ns: 3_000_000,
        capture_ns_last: 500_000,
        present_ns: 750_000,
        present_ns_last: 125_000,
        ..RuntimeProfile::default()
    };

    let report = profile.report();

    assert!(report.contains("present_enqueued=9"), "{report}");
    assert!(report.contains("frames_published=0"), "{report}");
    assert!(report.contains("commit_frames=0"), "{report}");
    assert!(report.contains("capture_frames=7"), "{report}");
    assert!(report.contains("capture_ms=3.000"), "{report}");
    assert!(report.contains("present_ms=0.750"), "{report}");
}
```

- [x] **Step 3: Verify the behavioral failure**

```bash
cargo nextest run -p wie-runtime \
  -E 'test(report_emits_all_present_path_counters_for_capture_only_session)'
```

Expected before report changes: the test panics because the present line is omitted for a capture-only profile.

- [x] **Step 4: Broaden the report condition**

```rust
if self.present_enqueued() > 0
    || self.frames_published() > 0
    || self.publish_ns_last() > 0
    || self.present_ns() > 0
    || self.present_ns_last() > 0
    || self.commit_frames() > 0
    || self.capture_frames() > 0
{
```

- [x] **Step 5: Extend the formatted line**

Use:

```rust
"present_enqueued={} frames_published={} publish_ms={:.3} publish_ms_last={:.3} \
 blit_copy_ms={:.3} blit_copy_ms_last={:.3} \
 present_ms={:.3} present_ms_last={:.3} \
 commit_frames={} commit_ms={:.3} commit_ms_last={:.3} \
 capture_frames={} capture_ms={:.3} capture_ms_last={:.3}",
```

The first format argument is `self.present_enqueued()`; all remaining arguments retain their current order.

- [x] **Step 6: Sample the channel counter**

Change `sample_frame_timing` to sample:

```rust
let (
    present_enqueued,
    channel_present_ns,
    channel_present_ns_last,
    commit_frames,
    commit_ns,
    commit_ns_last,
    capture_frames,
    capture_ns,
    capture_ns_last,
) = self.process.with_winapi_ref(|st| {
    st.try_present()
        .map(|p| {
            let channel = p.channel_arc();
            (
                channel.present_enqueued(),
                channel.present_ns(),
                channel.present_ns_last(),
                channel.commit_frames(),
                u128::from(channel.commit_ns()),
                u128::from(channel.commit_ns_last()),
                channel.capture_frames(),
                u128::from(channel.capture_ns()),
                u128::from(channel.capture_ns_last()),
            )
        })
        .unwrap_or((0, 0, 0, 0, 0, 0, 0, 0, 0))
});
```

Assign `self.profile.present_enqueued = present_enqueued;` beside the other present fields.

- [x] **Step 7: Verify the focused and module tests**

```bash
cargo nextest run -p wie-runtime \
  -E 'test(report_emits_all_present_path_counters_for_capture_only_session)'
cargo nextest run -p wie-runtime session::profile::tests
```

Expected: the new test and all runtime-profile unit tests pass.

---

### Task 3: Correct the Acceptance Harness

**File:**
- Modify: `scripts/acceptance-wave2.sh:1-137`

**Interfaces:**
- Consumes: the runtime report fields from Task 2 and the production CLI `run --gui --root <path> <pe>` mode.
- Produces: strict status, production-path, handler-budget, and baseline-write invariants.

**Validation owner:** CLI acceptance owner.

- [x] **Step 1: Add the failing bad-status smoke**

```bash
set +e
before="$(wc -l < docs/baselines/wave2-acceptance.txt 2>/dev/null || printf '0')"
output="$(CLI=/bin/false ./scripts/acceptance-wave2.sh 1 2>&1)"
status=$?
after="$(wc -l < docs/baselines/wave2-acceptance.txt 2>/dev/null || printf '0')"
set -e

test "$status" -eq 1
test "$after" -eq "$before"
grep -Fq "FAIL: unexpected emulator status 1" <<<"$output"
```

Expected before implementation: the final `grep` fails because the current script only warns about status `1`.

- [x] **Step 2: Correct the script contract comments**

Document these invariants:

```text
1. production wie run --gui capture path
2. capture_frames > 0 and capture_frames <= present_enqueued
3. present_ms > 0
4. handler_ms / present_enqueued <= 1 ms
5. process CPU is informational
```

Remove claims that the run is headless, divides `emu_ms` by legacy published frames, or requires 90% CPU.

- [x] **Step 3: Create isolated temporary paths**

```bash
TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/wave2-acceptance.XXXXXX")"
OUT="$TMP_ROOT/output.txt"
BOTTLE="$TMP_ROOT/bottle"
mkdir -p "$BOTTLE"
trap 'rm -rf "$TMP_ROOT"' EXIT
```

- [x] **Step 4: Prevent render-path opt-outs**

Before either launch branch:

```bash
unset WIE_CAPTURE_STREAM WIE_PRESENT_COMMIT
```

Do not add a headless fallback.

- [x] **Step 5: Launch the production GUI adapter**

Use in the timeout branch:

```bash
WIE_RUNTIME_PROFILE=1 WIE_GUEST_ENV="WIE_SELFTEST=2" \
  timeout --signal=INT --kill-after=5s "${SECS}s" \
  "$CLI" run --gui --root "$BOTTLE" "$PE" >"$OUT" 2>&1
```

Use in the fallback branch:

```bash
WIE_RUNTIME_PROFILE=1 WIE_GUEST_ENV="WIE_SELFTEST=2" \
  "$CLI" run --gui --root "$BOTTLE" "$PE" >"$OUT" 2>&1 &
```

Retain the existing fallback watchdog and cleanup.

- [x] **Step 6: Make process status a hard invariant**

```bash
case "$status" in
  124|130)
    ;;
  *)
    echo "FAIL: unexpected emulator status $status (expected 124 or 130)" >&2
    tail -n 30 "$OUT" >&2 || true
    exit 1
    ;;
esac
```

Extract the report with enough lines to include the complete counter row:

```bash
report="$(grep -A200 "WIE_RUNTIME_PROFILE" "$OUT" || true)"
```

- [x] **Step 7: Replace the invariant block**

```bash
fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

frames_published="$(sed -n 's/.*frames_published=\([0-9]*\).*/\1/p' <<<"$report" | tail -1)"
handler_ms="$(sed -n 's/.*handler_ms=\([0-9.]*\).*/\1/p' <<<"$report" | tail -1)"
present_ms="$(sed -n 's/.*present_ms=\([0-9.]*\).*/\1/p' <<<"$report" | tail -1)"
present_enqueued="$(sed -n 's/.*present_enqueued=\([0-9]*\).*/\1/p' <<<"$report" | tail -1)"
capture_frames="$(sed -n 's/.*capture_frames=\([0-9]*\).*/\1/p' <<<"$report" | tail -1)"
commit_frames="$(sed -n 's/.*commit_frames=\([0-9]*\).*/\1/p' <<<"$report" | tail -1)"
cpu_pct="$(sed -n 's/.*cpu%≈\([0-9.]*\).*/\1/p' <<<"$report" | tail -1)"

[[ -n "$handler_ms" ]] || fail "handler_ms missing from profile"
[[ -n "$present_ms" ]] || fail "present_ms missing from profile"
[[ -n "$present_enqueued" ]] || fail "present_enqueued missing from profile"
[[ -n "$capture_frames" ]] || fail "capture_frames missing from profile"
[[ -n "$commit_frames" ]] || fail "commit_frames missing from profile"

(( present_enqueued > 0 )) ||
  fail "present_enqueued is zero — no D3D9 Present boundary was sampled"
(( capture_frames > 0 )) ||
  fail "capture_frames is zero — production capture thread published nothing"
(( capture_frames <= present_enqueued )) ||
  fail "capture_frames=${capture_frames} exceeds present_enqueued=${present_enqueued}"

if ! python3 -c "import sys; sys.exit(0 if $present_ms > 0.0 else 1)"; then
  fail "present_ms=${present_ms} — winit/wgpu consumed no frame"
fi

handler_per_present="$(python3 -c "print(f'{$handler_ms / $present_enqueued:.3f}')")"
if python3 -c "import sys; sys.exit(0 if $handler_ms / $present_enqueued <= 1.0 else 1)"; then
  echo "PASS: handler time ${handler_per_present} ms/Present ≤ 1 ms" >&2
else
  fail "handler time ${handler_per_present} ms/Present > 1 ms"
fi

echo "PASS: capture_frames=${capture_frames} <= present_enqueued=${present_enqueued}" >&2
echo "PASS: present_ms=${present_ms}" >&2
echo "INFO: frames_published=${frames_published:-0} commit_frames=${commit_frames}" >&2
if [[ -n "$cpu_pct" ]]; then
  echo "INFO: process cpu%≈${cpu_pct}% (not a guest-thread acceptance gate)" >&2
fi
```

- [x] **Step 8: Move baseline append behind every hard check**

```bash
if [[ "${WAVE2_BASELINE:-0}" == "1" ]]; then
  mkdir -p "$OUTDIR"
  {
    printf '%s | %ss | present_enqueued=%s | capture_frames=%s | commit_frames=%s | frames_published=%s | handler_ms/present=%s | present_ms=%s | cpu%%=%s | ' \
      "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
      "$SECS" \
      "$present_enqueued" \
      "$capture_frames" \
      "$commit_frames" \
      "${frames_published:-0}" \
      "$handler_per_present" \
      "$present_ms" \
      "${cpu_pct:-unavailable}"
    tr '\n' ' ' <<<"$report" | sed 's/  */ /g'
    printf '\n'
  } >>"$BASELINE"
  echo "appended to $BASELINE" >&2
fi

exit 0
```

- [x] **Step 9: Verify shell syntax and bad-status behavior**

```bash
bash -n scripts/acceptance-wave2.sh
```

Repeat Step 1 and require all assertions to succeed. Also require:

```bash
rg -F 'unset WIE_CAPTURE_STREAM WIE_PRESENT_COMMIT' scripts/acceptance-wave2.sh
rg -F '"$CLI" run --gui --root "$BOTTLE" "$PE"' scripts/acceptance-wave2.sh
```

Expected: both static checks print a match.

---

### Task 4: Run Durable Correctness and Quality Gates

**Validation owners:** Rust unit-test owner, GUI micro-test owner, and repository gate owner.

- [x] **Step 1: Build the canonical fixture**

```bash
make -C micro-exes out/gui_d3d9.exe
```

Expected: `micro-exes/out/gui_d3d9.exe` exists.

- [x] **Step 2: Run the new durable unit tests**

```bash
cargo nextest run -p wie-winapi \
  -E 'test(test_d3d9_present_counts_entry_before_output_branch) or test(test_d3d9_present_publishes_surface_frame)'
cargo nextest run -p wie-runtime \
  -E 'test(report_emits_all_present_path_counters_for_capture_only_session)'
```

Expected: all selected tests pass without opening a GUI window.

- [x] **Step 3: Reuse the capture and commit hash oracles**

```bash
cargo nextest run -p wie-runtime \
  -E 'test(gui_d3d9_capture_stream_frame_matches_legacy_hash) or test(gui_d3d9_commit_thread_frame_matches_legacy_hash)'
```

Expected: both tests run rather than skip because the fixture was built in Step 1.

- [x] **Step 4: Run the repository gate**

```bash
./scripts/check.sh
```

Expected: formatting, clippy, workspace nextest, and micro-suite pass.

---

### Task 5: Run Native Acceptance, Then Record Only the Actual Outcome

**Validation owner:** Performance/acceptance owner on a logged-in macOS desktop.

- [x] **Step 1: Check preconditions**

Use a release `wie` binary, an idle unlocked machine, a logged-in Aqua session, no concurrent WIE GUI acceptance process, and `micro-exes/out/gui_d3d9.exe`. Do not substitute `--screenshot`, `--persistent`, micro mode, or a headless capture adapter.

- [x] **Step 2: Run the 40-second gate**

```bash
WAVE2_BASELINE=1 ./scripts/acceptance-wave2.sh 40
```

A passing run has status `124` or `130`, one profile report, `present_enqueued > 0`, `capture_frames > 0`, `capture_frames <= present_enqueued`, `present_ms > 0`, `handler_ms / present_enqueued <= 1.0`, script status `0`, and exactly one new passing baseline row.

If any invariant fails, stop without retaining a new baseline row or claiming success.

- [x] **Step 3: Verify a generated baseline**

```bash
git diff --check -- docs/baselines/wave2-acceptance.txt
git diff -- docs/baselines/wave2-acceptance.txt
```

Expected: one timestamped passing row with exact report values and no placeholder.

- [x] **Step 4: Update only current live status sections if Step 2 passed**

In `docs/handoff-wave2-next-steps.md`, update only the current close-out and current Step 3 with the actual counters and handler ratio. Preserve historical implementation records and dated notes.

In `docs/implementation-plan.md`, update only the Wave 0 harness row and Wave 2 acceptance row. Use `handler_ms / present_enqueued ≤ 1 ms`; do not alter reconciled Wave 3–6 rows.

In `docs/status.md`, update only the harness summary and Wave 2 acceptance bullet. Do not alter Wave 3–6 close-out bullets.

If Step 2 did not pass, record the exact blocker in those current sections and keep acceptance pending; do not weaken an invariant.

- [x] **Step 5: Final review**

```bash
git diff --check
git status --short
```

Review only the counter, profile, script, optional passing baseline, and three live-document changes. Do not commit automatically.
