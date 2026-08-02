# WIE Publish-Model Rework — Design (ora-5, verified Aug 2, 2026)

Status: approved design — implementation queued behind fix-49 (resize lane, in flight; its changes reconcile per §6).

## Verified root causes (file:line against the working tree)

1. **Per-dispatch publish splitting (PRIMARY)** — drains fire after every dispatch (`session.rs:1808`, `session.rs:2199-2201`, `mt_runtime.rs:391`); a WM_PAINT cycle spans owner erase + per-child paints (`controls.rs:226-238`), so the first published frame is the owner erase with black child holes; the host presents mid-cycle frames.
2. **Region delta vs skipped presents (DEEPEST FLAW)** — `frame.region` = delta since last *guest publish* (`present.rs:357`); wgpu staging = host's *last present*; B2 coalescing (`app.rs:696-703`) + `take_frame` returning only the latest frame silently drop intermediates → a losing publish's delta is permanently absent from the staging. Pure scheduling → run-to-run nondeterminism.
3. **Resize realloc garbage** — `pixels.resize` shrink truncates rows reinterpreting at new width (`present.rs:245-253`); settle invalidates only the owner (`app.rs:637-640`) → children never repaint in the resize cycle → `blue_in_button`.
4. **Erase-after-child** — mostly protected (WS_CLIPCHILDREN subtraction in `blit.rs:533-563,471,745,796`, `message.rs:407-433`, `dialog.rs:428-450`); audit `text.rs:500,741` + `fill_rect_surface` unclipped paths.

## Chosen model

- **Fix A — full-frame publishes always**: delete `SurfaceFrame.region`, `WindowSurface.dirty`, `mark_dirty`/`mark_dirty_full`, region derivation, wgpu `UploadRegion`. Staging invariant: *staging always = last published frame*. Cost ~4.096 MB/present ≈ 245 MB/s at 60 Hz — dwarfed by the 16.7 ms budget; B1 zero-copy and B2 gen+size skip preserved.
- **Fix B — drain at quiescence**: drain pending publishes at end of `run_until_stop` (`session.rs` before `sample_frame_timing`), reusing the gated helper (`session.rs:689-699`). Empty message queue ⟺ every invalidated window painted (`message.rs:967-997`). Remove drains at `session.rs:1808`, `2199-2201`, `mt_runtime.rs:391`. D3D9 `Present` keeps immediate `publish` (`d3d9.rs:2178`).
- **Resize**: new `invalidate_window_tree(state, owner)` on settle — invalidate every descendant so the resize cycle repaints the whole composite.
- **Trade-offs**: non-GetMessage parks delay the frame until the next idle (acceptable); ApiLimit mid-cycle drains publish a transient partial (self-healing); worker paints publish at next primary idle (one-frame latency, correct content).

## Migration steps (both hash gates green at every step)

| Step | Change | Risk |
|---|---|---|
| 1 | `publish()` always derives `region=None` (machinery inert) | none |
| 2 | Move drain to end-of-run; delete per-dispatch drains | low |
| 3 | `invalidate_window_tree` on settle | low |
| 4 | Delete region machinery + `SurfaceFrame.region` (~150 lines) | low |
| 5 | Audit unclipped paint paths (`text.rs:500,741`) | low |
| 6 | Regression test; retire `probe_publish.rs` | — |

## Status (implemented)

Steps 1–5 landed; the quiescent drain at `session.rs` (WaitingForMessage) is **unconditional** — it also fires while a guest callback is in flight. That is the fix for the interactive click-path dialog: `BM_CLICK → bridged WM_COMMAND → guest WndProc → DialogBoxParam` runs the in-guest modal `GetMessage` loop *inside* the callback, and skipping the drain there left the dialog's painted frame unpublished until the callback popped (dialog appeared only after clicking Exit). The empty-queue quiescence is the cycle-complete point regardless of callback nesting; full-frame publishes keep every emitted snapshot coherent.

Regression test landed as `gui_demo_dialog_opens_on_click` (`micro_gui_window.rs`): drives gui_demo's selftest, which opens the modal dialog through the real `BM_CLICK` click path (timer tick 1 clicks the button, tick 2 posts Enter → `IsDialogMessage` → DEFPUSHBUTTON OK → `EndDialog(1)`), and asserts (a) exit 0 (dialog text, edit echo, combo echo, list echo, incl. a UTF-8 round-trip through the A-string boundary) and (b) the dialog face (0xF0F0F0) appears in a published frame while the dialog is open. Pre-fix (a) failed at the dialog echo with the frame stuck; post-fix both hold.

Supporting fixes the selftest surfaced (all latent — the demo's selftest had never run headless before, the CLI `run` path does not inject `WIE_SELFTEST` into the guest env):
- `getdlgitem` (bare, no A/W suffix) added to `WINAPI_NAME_ROWS` — mingw imports it as-is.
- Dialog opens now set `active_window_handle` (real Windows: a modal dialog takes activation), so `GetActiveWindow()` returns the dialog.
- CB_* control messages (`CB_ADDSTRING/GETCURSEL/GETLBTEXT/SETCURSEL`) aliased to the shared LB_* handlers — previously fell through to `Ok(None)`.
- Demo dialog OK is now `DEFPUSHBUTTON` (Enter activates it); dialog text is `"dialog text — ✓"` exercising the UTF-8 A-string round-trip.

## Regression test

`gui_control_every_published_frame_is_complete`: drive gui_control one dispatch per run_until_stop, assert EVERY observed frame during the initial cycle is complete (button face, static face, blue background all correct). Fails pre-fix (first dispatch publishes owner erase with black child holes); passes post-fix. Run 3× under GUI_SUITE_LOCK with identical first-frame hashes (determinism pin). Existing `gui_blit` poll-until-hash CANNOT catch this (first-idle sampling passes today).

## Reconciliation with fix-46/fix-49 (in flight)

- fix-46's `publish_deferred`/`pending_publishes`/`drain_pending_publishes` machinery survives; its "one frame per dispatch" contract doc comments (`present.rs:144-154`, `session.rs:678-688`) are superseded by "one frame per quiescent cycle" — update in step 2.
- fix-49's `probe_publish.rs` per-dispatch sampling is exactly the step-6 regression harness — promote its assertion set ("every sampled frame complete, no blue_in_button"), then delete the throwaway file.
- Hash gates unaffected: both hash pixels only; `SurfaceFrame.region` removal doesn't touch them.
