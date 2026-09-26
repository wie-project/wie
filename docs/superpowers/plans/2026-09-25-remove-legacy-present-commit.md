# Remove Legacy Present-Commit Path Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Remove the superseded Wave 2 Present-commit render thread and its dead counters, controls, and test while preserving the production capture path and the headless legacy raster oracle.

**Architecture:** First move the shared `blit_frame_into` implementation out of the commit module without changing behavior. Then delete the commit pipeline at its actual boundaries: `PresentChannel`, D3D9 Present selection, `GuestHandle`, GUI startup, runtime profile reporting, the acceptance script schema, and the commit-only hash test. The capture stream and legacy in-handler D3D9 raster path remain; `WIE_CAPTURE_STREAM=0` remains the explicit escape hatch and headless/CI oracle.

**Tech Stack:** Rust, Bash, Python 3, `cargo nextest`, winit/wgpu, D3D9 software renderer, macOS logged-in GUI session.

**Spec:** `docs/implementation-plan.md:32-39`, `docs/RUNBOOK.md:125-140`, `docs/baselines/wave2-acceptance.txt`, and `docs/architecture-review-games-and-apps.md:173-175`.

## Global Constraints

- Do not remove the headless in-handler D3D9 raster path; it is the independent correctness oracle for capture snapshot completeness.
- Do not remove `WIE_CAPTURE_STREAM=0`; it is the explicit production bisect/rollback switch.
- Do not remove `WIE_CPU=iced`, degrade-not-die, `WIE_DEGRADE=0`, or any JIT/memory/string kill switches; they are differential-test oracles.
- Do not remove `frames_published`; it is the live GDI/GL/DIB publish counter, even though the D3D9 acceptance no longer uses it as a denominator.
- Do not rewrite historical handoff records or the existing baseline row; update only current live status text.
- Do not change capture replay, handback, RT read-back, or window-mirror behavior.
- Do not add dependencies, commits, or branch switches.
- Preserve strict workspace lints and the existing `--gui` production acceptance contract.
- A2 (inert `WIE_JIT_DIRECT_REGS` and unusable `WIE_JIT_TAILCHAIN` cleanup) is a separate plan after A1 validation.

---

### Task 1: Relocate the Shared Blit Helper Without Behavior Changes

**Files:**
- Create: `crates/wie-winapi/src/present/publish_tail.rs`
- Modify: `crates/wie-winapi/src/present/mod.rs:12-19,1093-1101`
- Modify: `crates/wie-winapi/src/present/stream.rs:25-27`
- Modify: `crates/wie-winapi/src/present/commit.rs:339-391`

**Interfaces:**
- Consumes: the existing `blit_frame_into` implementation used by `PresentState::blit_frame` and `capture_thread`.
- Produces: `present::publish_tail::blit_frame_into`, with the same signature and byte-for-byte behavior.

**Validation owner:** WinAPI/present owner.

- [x] **Step 1: Record the characterization test before moving the helper**

```bash
cargo nextest run -p wie-runtime --test micro_gui_window \
  -E 'test(gui_d3d9_capture_stream_frame_matches_legacy_hash) or test(demo)'
```

Expected before the refactor: the capture hash gate and the existing demo gates pass. These tests are the behavioral oracle; this task must not change their expected hashes.

- [x] **Step 2: Create `publish_tail.rs` with the exact moved helper**

Create `crates/wie-winapi/src/present/publish_tail.rs` containing:

```rust
//! Shared blit tail for the D3D9 capture and legacy in-handler present paths.

/// Blit a whole 0RGB frame into a destination surface buffer.
///
/// A frame sized exactly like the destination copies row-major (one
/// `copy_from_slice` when the pitch matches, per-row otherwise — the surface
/// pitch may be 64-padded, the source frame is not); anything else is
/// nearest-neighbour stretched to the destination dimensions.
pub(crate) fn blit_frame_into(
    dst: &mut [u32],
    dst_stride: u32,
    dst_width: u32,
    dst_height: u32,
    frame: &[u32],
    frame_width: u32,
    frame_height: u32,
) {
    if frame_width == dst_width && frame_height == dst_height {
        if dst_stride == dst_width {
            let n = dst.len().min(frame.len());
            if let (Some(d), Some(s)) = (dst.get_mut(..n), frame.get(..n)) {
                d.copy_from_slice(s);
            }
        } else {
            // Pitched destination: copy each logical row at its stride.
            let stride = usize::try_from(dst_stride).unwrap_or(0);
            let width = usize::try_from(dst_width).unwrap_or(0);
            let height = usize::try_from(dst_height).unwrap_or(0);
            for row in 0..height {
                let src_start = row.saturating_mul(width);
                let dst_start = row.saturating_mul(stride);
                let (Some(src), Some(d)) = (
                    frame.get(src_start..src_start.saturating_add(width)),
                    dst.get_mut(dst_start..dst_start.saturating_add(width)),
                ) else {
                    break;
                };
                d.copy_from_slice(src);
            }
        }
    } else {
        wie_cpu::stretch_nearest_strided(
            dst,
            dst_stride,
            frame,
            frame_width,
            frame_height,
            dst_width,
            dst_height,
        );
    }
}
```

- [x] **Step 3: Redirect both callers and remove the duplicate definition**

In `present/mod.rs`, add `mod publish_tail;` beside the other present submodules and change the `PresentState::blit_frame` call to:

```rust
publish_tail::blit_frame_into(
```

In `present/stream.rs`, change:

```rust
use super::commit::blit_frame_into;
```

to:

```rust
use super::publish_tail::blit_frame_into;
```

Delete only the old `blit_frame_into` definition from `present/commit.rs`; keep the commit module and its thread temporarily so this task is a pure relocation.

- [x] **Step 4: Verify the refactor**

```bash
cargo nextest run -p wie-runtime --test micro_gui_window \
  -E 'test(gui_d3d9_capture_stream_frame_matches_legacy_hash) or test(demo)'
cargo fmt --all --check
```

Expected: identical passing tests and no formatting changes beyond the new module.

---

### Task 2: Delete the Superseded Commit Pipeline

**Files:**
- Delete: `crates/wie-winapi/src/present/commit.rs`
- Modify: `crates/wie-winapi/src/present/mod.rs:12-19,212-224,350-381,1106-1135`
- Modify: `crates/wie-winapi/src/d3d9/device.rs:348-401`
- Modify: `crates/wie-runtime/src/session/window/mod.rs:698-725`
- Modify: `crates/wie-cli/src/gui/app.rs:970-988`
- Modify: `crates/wie-runtime/src/session/profile.rs:34-54,206-220,444-472,602-672`
- Delete: `crates/wie-runtime/tests/micro_gui_window/commit.rs`
- Modify: `crates/wie-runtime/tests/micro_gui_window/main.rs:11-13`

**Interfaces:**
- Consumes: the relocated `publish_tail::blit_frame_into` from Task 1.
- Produces: a present channel with only capture and window-mirror pipelines; D3D9 Present selects capture first and otherwise uses the legacy in-handler path.

**Validation owner:** WinAPI/runtime/CLI owner.

- [x] **Step 1: Remove the commit module and channel surface**

In `present/mod.rs`, remove `mod commit;` and `pub use commit::{CommitterHandle, spawn_present_committer};`. Remove `commit: commit::PresentCommit` from `PresentChannel` and its initialization. Remove these methods:

```rust
pub fn set_commit_enabled(&self, enabled: bool)
pub fn commit_enabled(&self) -> bool
pub fn commit_ns(&self) -> u64
pub fn commit_ns_last(&self) -> u64
pub fn commit_frames(&self) -> u64
```

Remove `enqueue_present_commit` from `PresentState`. Do not remove `publish_tail::blit_frame_into` or the capture field.

- [x] **Step 2: Remove the D3D9 commit branch**

In `d3d9/device.rs`, keep the capture `if` at `:348-358`. Replace the current `else if` commit branch plus its legacy `else` with the legacy body only: retain the client-size calculation, `ensure_surface`, `blit_frame`, and backbuffer restoration from `:385-400`. The resulting shape must be:

```rust
if state.present().channel.capture_enabled() {
    // existing capture flush path
} else if bb_w > 0
    && bb_h > 0
    && hwnd != crate::handles::Hwnd::NULL
    && !state.d3d9().d3d9_backbuffer.is_empty()
{
    // existing legacy in-handler blit_frame path
}
```

- [x] **Step 3: Remove runtime and GUI startup APIs**

Delete `GuestHandle::enable_present_commit` from `session/window/mod.rs`. Delete the `_present_committer` block from `gui/app.rs:970-988`; keep the capture streamer block and its default-on/opt-out behavior unchanged. Update nearby comments so they describe capture as the only GUI render-thread path.

- [x] **Step 4: Remove commit-only profile plumbing**

From `session/profile.rs`, remove the `commit_frames`, `commit_ns`, and `commit_ns_last` fields and their getters. Remove `commit_frames` from the report condition and remove all `commit_*` fields/arguments from the formatted present line. In `sample_frame_timing`, remove the three commit tuple elements, their channel reads, and their assignments. Keep `present_enqueued`, `frames_published`, `present_*`, `capture_*`, and all Wave 4 `degraded_insns` data.

The resulting report line must contain:

```text
present_enqueued={} frames_published={} publish_ms={:.3} publish_ms_last={:.3} \
 blit_copy_ms={:.3} blit_copy_ms_last={:.3} \
 present_ms={:.3} present_ms_last={:.3} \
 capture_frames={} capture_ms={:.3} capture_ms_last={:.3}
```

- [x] **Step 5: Delete the commit-only test module**

Delete `crates/wie-runtime/tests/micro_gui_window/commit.rs` and remove `mod commit;` from `micro_gui_window/main.rs`. Do not delete `capture.rs` or the demo hash tests.

- [x] **Step 6: Delete `present/commit.rs` after the call-site search is clean**

Run:

```bash
rg -n 'commit::|CommitterHandle|spawn_present_committer|enable_present_commit|commit_enabled|enqueue_present_commit|commit_frames|commit_ns|commit_ns_last' \
  crates scripts docs/RUNBOOK.md
```

Expected before deletion: only the planned commit module, its call sites, the commit test, and the acceptance/profile references identified above. After removing those references, delete `crates/wie-winapi/src/present/commit.rs`.

- [x] **Step 7: Verify the surviving GUI capture and headless legacy paths**

```bash
cargo nextest run -p wie-runtime --test micro_gui_window \
  -E 'test(gui_d3d9_capture_stream_frame_matches_legacy_hash) or test(demo)'
cargo nextest run -p wie-winapi \
  -E 'test(test_d3d9_present_counts_entry_before_output_branch) or test(test_d3d9_present_publishes_surface_frame)'
cargo fmt --all --check
```

Expected: the capture hash and headless legacy hash oracles pass, Present-entry accounting still passes, and formatting is clean.

---

### Task 3: Update the Acceptance Schema and Live Documentation

**Files:**
- Modify: `scripts/acceptance-wave2.sh:101-156`
- Modify: `docs/RUNBOOK.md:125-140`
- Modify: `docs/implementation-plan.md:32-39`
- Modify: `docs/status.md:38-49`
- Modify: `docs/handoff-wave2-next-steps.md:5-24,70-88`
- Modify: `docs/superpowers/plans/2026-09-25-wave2-acceptance-gate.md:13-19`

**Interfaces:**
- Consumes: the capture-only profile format produced by Task 2.
- Produces: an acceptance script and live docs that no longer require or advertise the deleted commit path.

**Validation owner:** CLI/docs owner.

- [x] **Step 1: Remove commit fields from the acceptance parser and baseline format**

In `scripts/acceptance-wave2.sh`, delete the `commit_frames` extraction, its non-empty assertion, the commit portion of the INFO line, and the commit argument/field from the baseline `printf`. Keep `frames_published` as informational GDI/GL observability; do not use it as the D3D9 denominator. Keep these hard checks unchanged:

```text
present_enqueued > 0
capture_frames > 0
capture_frames <= present_enqueued
present_ms > 0
handler_ms / present_enqueued <= 1 ms
```

- [x] **Step 2: Update the RUNBOOK without removing required fallbacks**

Delete the `WIE_PRESENT_COMMIT` row. Add or retain a row for `WIE_CAPTURE_STREAM=0` documenting that it selects the headless-compatible legacy in-handler raster path. Keep the existing rows for `WIE_JIT_*`, `WIE_CPU=iced`, and degrade controls. Remove the `WIE_JIT_DIRECT_REGS` claim only in the separate A2 plan, not in A1.

- [x] **Step 3: Update current live status text**

Change current Wave 2 text to state that the capture render thread is the sole GUI render-thread path; remove claims that a commit thread is spawned or that `commit_frames` is a live acceptance counter. Preserve the accepted baseline evidence and the historical handoff sections. Update the old Wave 2 plan's constraint that says to keep the commit hash test so it accurately reflects the surviving capture oracle.

- [x] **Step 4: Verify docs and script statically**

```bash
bash -n scripts/acceptance-wave2.sh
rg -n 'WIE_PRESENT_COMMIT|CommitterHandle|spawn_present_committer|enable_present_commit|commit_frames' \
  crates scripts docs/RUNBOOK.md docs/implementation-plan.md docs/status.md
```

Expected: no current-code or current-live-doc matches. Historical handoff text may retain dated references if clearly labeled historical.

- [x] **Step 5: Run the bad-status smoke without a baseline append**

Use a temporary executable that exits `1` and `WAVE2_BASELINE=1`; require exit `1`, the `FAIL: unexpected emulator status 1` message, and an unchanged baseline line count. This proves the schema change did not weaken failure handling.

---

### Task 4: Validate the Removal and Rerun Native Acceptance

**Validation owners:** Runtime test owner, repository gate owner, and native GUI acceptance owner.

- [x] **Step 1: Run focused correctness tests**

```bash
make -C micro-exes out/gui_d3d9.exe
cargo nextest run -p wie-runtime --test micro_gui_window \
  -E 'test(gui_d3d9_capture_stream_frame_matches_legacy_hash) or test(demo)'
cargo nextest run -p wie-winapi \
  -E 'test(test_d3d9_present_counts_entry_before_output_branch) or test(test_d3d9_present_publishes_surface_frame)'
```

Expected: all selected tests pass, including the headless legacy oracle.

- [x] **Step 2: Run the repository gates**

```bash
./scripts/check-file-sizes.sh
cargo fmt --all --check
cargo clippy --workspace --all-targets
cargo nextest run --workspace
make -C micro-exes
./scripts/run-micro-suite.sh
```

Expected: every step passes. Do not accept a hash-test skip as proof; build the fixture first.

- [x] **Step 3: Run the native release acceptance**

```bash
cargo build -p wie-cli --release
WAVE2_BASELINE=1 ./scripts/acceptance-wave2.sh 40
```

Expected: exit `0`; `present_enqueued > 0`; `capture_frames > 0`; `capture_frames <= present_enqueued`; `present_ms > 0`; handler ratio at most `1.0`; no `commit_frames` key in the new report; one new capture-only baseline row.

- [x] **Step 4: Final diff and status review**

```bash
git diff --check
git status --short --branch
rg -n 'WIE_PRESENT_COMMIT|CommitterHandle|spawn_present_committer|enable_present_commit|commit_frames' \
  crates scripts docs/RUNBOOK.md
```

Expected: only the intended present-domain, profile, script, test, and live-doc changes; no current commit-pipeline references. Leave the change uncommitted for review.

---

## Self-Review Checklist

- Spec coverage: Tasks 1–4 cover shared-helper relocation, commit deletion, acceptance/docs schema, and verification.
- Placeholder scan: no `TBD`, `TODO`, or unspecified implementation steps; every code change names exact files and behavior.
- Type consistency: `blit_frame_into` keeps the original signature; capture-only profile fields remain `present_enqueued`/`capture_*`; no removed commit type is referenced by a later task.
- Oracle preservation: headless legacy raster, `WIE_CAPTURE_STREAM=0`, `iced`, degrade-not-die, kill switches, and `frames_published` remain explicit constraints.
