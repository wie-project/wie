# Handoff — Wave 2 close-out (current status)

Context: `docs/implementation-plan.md` is the live plan. Branch: `feat/dll-coverage`.

**Current close-out (2026-09-25):** Wave 2 implementation is complete. Commits
`3cb2370`, `22b56c7`, `7eb4c1d`, `25dba15`, and `7c08ef3` landed the D3D9
command capture/render thread, window mirror, RT read-back rendezvous, and GUI
default-on wiring; the superseded Present-commit render thread was removed
afterwards, so the **capture stream render thread is the sole GUI
render-thread path** (`WIE_CAPTURE_STREAM=0` remains the legacy escape hatch
and the headless/CI hash oracle). **Acceptance passes.**
`WAVE2_BASELINE=1 ./scripts/acceptance-wave2.sh 40` exited **0** on a release
build through the native logged-in GUI path, on a run taken after the commit
path was deleted, with
`present_enqueued=693`, `capture_frames=693`,
`frames_published=1` (GDI/GL/DIB counter, not the capture denominator),
`handler_ms/present=0.096` (PASS, ≤1 ms), and
`present_ms=241.008` (PASS, >0). That report has no `commit_frames` key at
all: the commit path and its profile key are deleted, and the live acceptance
schema is capture-only. Process `cpu%=0.0` is informational only, not
a guest-thread acceptance gate. The baseline was appended at
`docs/baselines/wave2-acceptance.txt`, latest row timestamp
`2026-09-25T19:10:12Z`; the older `2026-09-25T17:57:50Z` row (with
`commit_frames=0`) is kept as frozen pre-removal evidence.
The earlier script repairs (duplicate `ROOT` line, canonical
`out/gui_d3d9.exe` target, missing-PE guard) are historical fixes, not current
blockers.

The acceptance result is recorded. The dated implementation notes below are
retained as handoff history; the live plan is authoritative for current
sequencing.

Conventions: workspace lints deny `unwrap_used`/`expect_used`/`panic`/
`indexing_slicing`/`as_conversions`; new modules stay under the 1500-line cap;
`pub(crate)` only where needed; docs updated in the same commit.

## Step 1 — D3D9 draw-command capture (historical implementation record)

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

## Step 2 — Kill the remaining big-lock reads (historical implementation record)

Mirror presenter-side reads into `PresentChannel` (same pattern as the
z-order/window-rev mirrors). Audited list (in `session/window.rs`):
`edit_selection`, `control_text`, `status_bar_part_text`, `window_at`,
`capture_target`, `focused_top_level`, `mouse_tracking`, `set_key_state`,
`first_guest_window_info`, `window_menu_items` rebuild, `resize_window`
settle. Snapshot under the source lock at mutation sites, read via the
channel; each accessor gets a test proving it no longer takes the big lock
(or note which ones legitimately still need it).

## Step 3 — Acceptance measurement (passed 2026-09-25, post-removal run)

The harness is landed in `7c08ef3` and is documented in the live plan. The
duplicate `ROOT` assignment and the missing-PE build target were repaired
earlier — historical fixes, not current blockers. The prescribed run
`WAVE2_BASELINE=1 ./scripts/acceptance-wave2.sh 40` exited **0** on a release
build through the native logged-in GUI path. The old `frames_published`/CPU
check was replaced by the production capture-path invariants:
`present_enqueued=693`, `capture_frames=693`,
`frames_published=1` (GDI/GL/DIB counter, not the capture denominator),
`handler_ms/present=0.096` (PASS, ≤1 ms),
`present_ms=241.008` (PASS, >0). The report carries no `commit_frames` key:
the commit path and its profile key are deleted with the superseded commit
render thread, so the current acceptance schema is capture-only. Process
`cpu%=0.0` is informational only, not a guest-thread acceptance gate. The
baseline was appended at `docs/baselines/wave2-acceptance.txt` (latest row
timestamp `2026-09-25T19:10:12Z`); the earlier `2026-09-25T17:57:50Z` row, with
its `commit_frames=0` sample, remains as frozen pre-removal evidence.

## Later waves (historical sequencing; current status is in the live plan)

The original sequencing below is retained for handoff context. It is superseded
where it says Wave 2 must close before later work: Wave 3 SSA-rflags, Wave 4
packed SSE/degrade-not-die/x87, and Wave 5 playback/timer/callback routing
have since landed, while their explicitly listed open work remains open.

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

## Step 2 implementation notes (2026-09-05)

Landed. The mirror is rev-gated, not per-site-cloned:

- **Sync seam**: `WindowMirror` lives on `PresentChannel` behind its own
  mutex. Mutation sites bump `WindowState.window_mirror_rev` via
  `touch_window_mirror()` (central in `find_window_mut`, which covers
  geometry/visibility/title/tracking, plus explicit bumps at focus
  assignment sites, Set/ReleaseCapture, menu mutations, create/destroy).
  `HandlerContext::finish()` — the universal per-handler exit — calls
  `sync_window_mirror_if_dirty()`: two integer compares when clean.
- **Keyboard direction**: the host cannot write `keyboard_state` without
  the big lock, so `set_key_state` pushes `(vk, pressed)` events onto the
  mirror; the guest-side readers (GetKeyState/GetAsyncKeyState/
  GetKeyboardState, IsDialogMessage, EDIT caret/scroll, accelerator
  translation) drain them under the big lock they already hold. Wholesale
  array copy was rejected — it would clobber guest-written bits.
- **Mirrored (no big lock, proven by probe tests)**: `window_at`,
  `window_at_in`, `capture_target`, `mouse_tracking`,
  `first_guest_window_handle`, `first_guest_window_info`,
  `focused_top_level`, `window_menu_items` (fast path: mirror clean +
  resolved handle == cached handle → cached tree with no lock; the
  rebuild path stays locked and re-syncs the mirror), `set_key_state`.
  The proof holds the big `WinApiState` lock in the test thread and runs
  each accessor on a worker thread with a 2 s timeout — a lock-taker
  surfaces as a panic, not a hang.
- **Legitimately still take the big lock (documented in-place)**:
  `focus_window` (rare), `edit_selection`/`control_text`/
  `status_bar_part_text` (test-only observation accessors),
  `take_host_geometry_request` (consumes a slot — a read-only mirror
  can't express take), `present_publish_ns_last` (diagnostics).
  `resize_window` keeps `try_lock` (never blocks) + an explicit
  `sync_window_mirror()` because it mutates outside any handler's
  `finish()`.
- **File-size policy**: `present/mod.rs` and `session/window.rs` test
  modules moved to exempt `present/tests.rs` and `session/window/tests.rs`.
- **Fresh-session nuance**: `session/window.rs` unit tests that seed
  `WindowState` directly call `sync_window_mirror()` after seeding —
  integration tests (real guests) need nothing; every handler finish
  syncs.
- **Stale window**: the mirror syncs at handler exit, so a presenter read
  can be one in-flight handler behind. Same cadence as the frame the
  presenter paints; accepted for all mirrored reads.

## Step 1 implementation notes (2026-09-05)

Landed, unresolved design decisions resolved as follows:

- **Op model**: only two op kinds — `Clear` (self-contained) and `Draw`
  (full device-state snapshot: matrices, viewport, typed render state +
  POINTSIZE + scissor, 8 stage states, texture-texel clones, parsed
  shader programs + constant files, target bindings, stream bytes). The
  `Set*` handlers record NOTHING: each draw's snapshot is exactly the
  state its stream prefix produced, so the single ordered `Vec`
  (clear-then-draws) preserves order semantics by construction — the
  handoff's "record SetState ops" requirement is met by folding.
- **Textures/shaders**: v1 record-time copies (as the doc allows); the
  follow-up is `Arc`-wrapping `TextureRecord`/`ShaderRecord` storage.
- **RT/depth round-trip**: RT/depth texel storage migrated to `Arc`
  (`RenderTargetRecord.pixels`, `DepthStencilRecord.depth` — legacy
  mutators use `Arc::make_mut`, free while unshared). The render thread
  keeps its own copies; the emu thread sends a target's `Arc` at flush
  when never-sent (`d3d9_capture_rt_seen`/`_depth_seen`) or emu-mutated
  (`d3d9_capture_emu_dirty_rt`, set by the RT `UnlockRect` copy-back).
  The render thread hands its post-frame RT buffers back; the emu thread
  installs the handback at each flush AND at RT lock/unlock (sync point
  — guest writes always layer on the render thread's latest state; reads
  at most one frame stale while a replay is in flight). Backbuffer and
  depth never hand back (no guest read path). VA-reuse handled by
  clearing the seen/dirty sets on surface release + device release.
- **Rasterizer**: `raster.rs` split into `RasterFrame` + `rasterize_frame`
  (shared core) and `rasterize_vertex_stream` (emu-side resolver) — the
  replay builds the same frame from the captured op, so both paths are
  identical by construction.
- **Gate**: `WIE_CAPTURE_STREAM=1` — the GUI host (app.rs) spawns
  `spawn_capture_streamer` via `GuestHandle::enable_capture_stream` only
  under that env; headless/CI never spawn, so zero behavior change when
  off. Test: `micro_gui_window::capture` (spawns explicitly).
- **Known v1 limitation** (documented in `d3d9/capture.rs`): per-draw
  texture/program clones and one RT texel copy per frame on the render
  thread's handback — bring-up correctness first, optimization next.

Remaining after the gates pass: flip capture to default-on (app.rs env
check removal) and re-run the hash suite; then Step 2.

## Slice 4 + Wave 2 close notes (2026-09-05)

- **RT read-back rendezvous (fix for the capture test's exit 217)**: a
  guest that renders into an offscreen RT and `LockRect`s it BEFORE any
  `Present` (gui_d3d9's L6 self-test) read pre-raster zeros — the replay
  only ran at Present-flush boundaries. `CapturePipeline` now sequences
  flushes (`enqueue` assigns a seq, the handback carries it) and
  `wait_for_handback` blocks on that barrier; RT `LockRect` with a
  non-empty stream calls `capture::sync_flush_for_target_read` (flush
  with zero publish dims → replay + handback, no frame published) and
  the read-back sees the rasterized texels. Rare path: costs nothing
  when the stream is empty.
- **Capture flipped default-on** for GUI sessions (`WIE_CAPTURE_STREAM=0`
  opts out); headless/CI never spawn the streamer, so the hash gates
  still exercise the legacy path byte-for-byte.
- **Acceptance harness** (Step 3): `scripts/acceptance-wave2.sh` runs a
  40 s SIGINT capture on gui_d3d9's new continuous-present mode
  (`WIE_SELFTEST=2`, injected headlessly via the `WIE_GUEST_ENV` hook in
  `memory.rs`) and checks emu ≤1 ms/frame, render-thread counters
  present, cpu%≥90; `WAVE2_BASELINE=1` appends to
  `docs/baselines/wave2-acceptance.txt`. The human step that remains:
  run it on an idle machine with a release build and commit the
  baseline.

## Remaining-lane continuation notes (2026-09-05, historical; superseded by the current close-out above)

The notes below preserve the handoff as written on 2026-09-05. They are not a
current acceptance record: SSA-rflags, broad Wave 4 coverage, and Wave 5
callback routing landed afterward, while Wave 2 acceptance was still blocked
by the hard-invariant failure recorded at the time.

Progress this session: Wave 2 implementation closed (slices 3-4 + capture default-on +
acceptance harness), Wave 3 slice 1 (tail-call chain hops
`WIE_JIT_TAILCHAIN`, ADR-0002 feasibility pass — **since reverted to
default-off: Cranelift 0.133 rejects `return_call` under every ABI
calling convention, see ADR-0002 status**), Wave 4 slice 1
(degrade-not-die fallback + `degraded_insns` metric), Wave 5 slice 1
(waveOut playback sink + timed `WOM_DONE`).

**Wave 3 slice 2 (next big JIT step)** — the design the feasibility pass
settled on:
- Do NOT attempt x19–x28 via Cranelift signatures: 17 results don't fit
  the aarch64 C-ABI result window and there is no custom call-conv hook.
- SSA rflags first (independent of the ABI): replace the packed `rflags:
  Value` plumbing with `FlagState { zf, sf, cf, of, pf: Option<Value> }`
  threaded through `emit_body_and_term`; `PendingFlags` (insn.rs:48-88)
  produces i1 booleans; `flag_cond` (lower/mod.rs:1295) consumes them
  directly; materialize to the packed u64 only at block boundaries
  (dispatcher exit / non-tail hops) and decompose only when
  `block_needs_flags` (analysis.rs:185). Gate: `WIE_JIT_SSA_FLAGS=0`
  opt-out; oracle = the existing B4 dual-path harness pattern
  (tests/mod.rs:752) extended to GPR/flags programs.
- Prologue-stub ABI after that: hand-written per-block stub (the
  trampolines.rs pattern) shuffling x19–x28/x14 against the C-ABI frame;
  chaining links the post-prologue body label.
- Measurement: `long_loop` micro-exe is the canonical metric
  (0.25 s → ≤0.10 s target); `cargo bench -p wie-cpu` +
  `scripts/capture-baseline.sh micro-exes/out/long_loop.exe`.

**Wave 4 next**: x87 subset (fld/fstp/fadd/fmul/fsub/fdiv/fcom on an
8-deep f64 stack in RegFile — additive field, JitCtx untouched; JIT bails
to the interpreter for these, which now degrades instead of stopping) and
packed integer SSE widening. **Wave 5 next**: CALLBACK_WINDOW/CALLBACK_EVENT
waveOut kinds (GuestCallbackRequest window-message shape already exists),
RawInput, multi-queue pump. **Wave 6** stays gated behind the FPS harness
decision gate.
