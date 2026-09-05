# Handoff — Wave 2 remaining steps (ordered)

Context: `docs/implementation-plan.md` is the live plan. Wave 1 done, Wave 2
slice 1 (D3D9 Present-commit render thread, `present/commit.rs`) landed in
`3cb2370`. Branch: `feat/dll-coverage`.

**Rule: do NOT run tests yourself.** Implement, then tell the user which
commands to run and stop. The user runs the gates and reports results.

Conventions: workspace lints deny `unwrap_used`/`expect_used`/`panic`/
`indexing_slicing`/`as_conversions`; new modules stay under the 1500-line
cap; `pub(crate)` only where needed; docs updated in the same commit.

## Step 1 — D3D9 draw-command capture (the core of Wave 2)

Goal: `Draw*` handlers stop rasterizing on the emu thread; they append
self-contained draw ops to a per-device stream. `Present` (already committed
to the render thread — see `present/commit.rs`) flushes the stream; the
committer replays the ops into its own backbuffer, then publishes as today.

- Key files: `crates/wie-winapi/src/d3d9/{draw,raster,device}.rs`,
  `crates/wie-winapi/src/state/d3d9.rs`, new capture module under
  `crates/wie-winapi/src/d3d9/` (or `present/` — keep under the size cap).
- What each op must snapshot (the rasterizer, `raster.rs::rasterize_vertex_stream`,
  reads all of this off `D3D9State`): vertex/index bytes (buffer form already
  clones them — keep; UP form reads guest memory at record time), FVF layout +
  stride + primitive groups, world/view/projection matrices, viewport,
  `RenderState` clone + raw POINTSIZE, scissor rect, stage states + bound
  texture pixels, bound VS/PS program + constant registers, depth-stencil
  buffer state.
- Hard problems, resolve explicitly before coding:
  1. Textures/shaders are borrowed from `D3D9State` maps today — wrap their
     pixel/program storage in `Arc` so an op snapshot is a cheap clone, or
     copy at record time (v1: copy is acceptable, optimize later).
  2. Render targets + depth buffers are mutated in place across frames and
     the GUEST reads them back (`LockRect`, `GetRenderTargetData`). Simplest
     correct model: the render thread keeps its own copy, synced back to
     `D3D9State` at the next flush boundary; design this round-trip first.
  3. Order between Clear/SetState/Draw ops must be preserved exactly — the
     stream is a single ordered Vec, no reordering.
- Opt-in gate `WIE_CAPTURE_STREAM=1` while bring-up (default off), flip to
  default-on only after the hash tests pass; keep the legacy synchronous
  raster path intact for headless/CI.
- Hash-equivalence test: extend `crates/wie-runtime/tests/micro_gui_window/`
  with a test that runs `gui_d3d9.exe` with the capture path enabled and pins
  `D3D9_RESTING_FRAME_HASH` (model: `commit.rs` test).

## Step 2 — Kill the remaining big-lock reads (Wave 2 item 3)

Mirror presenter-side reads into `PresentChannel` (same pattern as the
z-order/window-rev mirrors). Audited list (in `session/window.rs`):
`edit_selection`, `control_text`, `status_bar_part_text`, `window_at`,
`capture_target`, `focused_top_level`, `mouse_tracking`, `set_key_state`,
`first_guest_window_info`, `window_menu_items` rebuild, `resize_window`
settle. Snapshot under the source lock at mutation sites, read via the
channel; each accessor gets a test proving it no longer takes the big lock
(or note which ones legitimately still need it).

## Step 3 — Acceptance measurement

Land the FPS harness (see Wave-0 row in `implementation-plan.md`) into the
local capture protocol, then run the 40 s `WIE_RUNTIME_PROFILE=1` capture on
the D3D9 loop micro (`gui_d3d9.exe` / doomretro) and check: guest >90% CPU,
emu thread ≤1 ms/frame on D3D9 handlers, `commit_ms` separate from guest
time. Commit the baseline to `docs/baselines/`.

## Then (later waves, do not start before Wave 2 closes)

- Wave 3: ADR-0002 direct-register ABI + SSA rflags → block chaining →
  multi-module compile (`long_loop` 0.25 s → ≤0.10 s).
- Wave 4: per-instruction fallback on unknown mnemonics (no session stop),
  x87 subset, packed integer SSE.
- Wave 5: waveOut ring buffer + audio thread, RawInput, multi-queue pump.

## Gates — commands the USER runs after each step

```sh
./scripts/check.sh          # fmt + clippy + workspace nextest + micro-suite
cargo nextest run -p wie-runtime --test micro_gui_window   # CI hashes
```

Step 1 additionally (before the capture gate flips to default-on):

```sh
cargo nextest run -p wie-runtime -E 'test(gui_d3d9) or test(commit_thread)'
```

Report all failures verbatim to the agent; do not fix or rerun silently.
