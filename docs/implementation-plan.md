# WIE implementation plan — waves toward high-FPS emulation of any app

Source analysis: `docs/architecture-review-games-and-apps.md` (painpoint
catalog A–E and decision options). This file is the sequenced, executable
version of that review's roadmap, with live status. Update the status column
as waves land; do not restate rationale here.

Conventions that apply to every wave: workspace lints deny
`unwrap_used`/`expect_used`/`panic`/`indexing_slicing`/`as_conversions`
(use `?`, `.get()`, `try_from`); `unsafe` only in `wie-cpu`; pre-PR gate
`./scripts/check.sh`; micro-suite `make -C micro-exes &&
./scripts/run-micro-suite.sh`; all timing on release builds only.

## Wave 0 — drift audit + measurement (before anything moves)

| Item | Status |
| --- | --- |
| Docs-vs-code drift audit (every ADR claim → grep-verified) | ✅ done 2026-09-02 — ADR-0001 partially unbuilt (padding, scratch), ADR-0002 entirely unbuilt (dead gate); status notes appended to both ADRs; `docs/status.md` + `docs/RUNBOOK.md` claims corrected |
| ADR-0003 for the present channel | ✅ written (`docs/adr/0003-present-channel.md`) |
| End-to-end FPS harness (release build, headless windowed run, frames/s + present/jit counters, saved under `docs/baselines/`) | 🟡 **wired 2026-09-05** — `scripts/fps-harness.sh` (exit-on-quit counters) + `scripts/acceptance-wave2.sh` (40 s SIGINT-watchdog capture, `WIE_GUEST_ENV=WIE_SELFTEST=2` continuous-present mode in gui_d3d9, invariants checked: emu ≤1 ms/frame, commit/capture counters present, cpu%≥90; `WAVE2_BASELINE=1` appends to `docs/baselines/wave2-acceptance.txt`). The nightly-bench CI job was dropped in `2e97b0c` (manual-dispatch `.github/workflows/bench.yml`); the developer must RUN the capture to fill the baseline |
| Full workspace + micro suite green after each wave | ⬜ re-verify per wave |

## Wave 1 — present-domain decoupling (done 2026-09-02)

| Item | Status |
| --- | --- |
| 1a. `PresentChannel`: latest-wins slot, wake gate (`published_seq`/`taken_seq`), presenter reads without the big lock | ✅ — `wie-winapi/src/present/mod.rs`, `GuestHandle` wired; hand-back now zero-ALLOC via the channel's spare pool (displaced slots + presented buffers); `pending_frames()` re-arm wired into `RedrawRequested` |
| 1b. ADR-0001: 64-px pitch padding (`padded_stride`, `WIE_SURFACE_PAD=0` opt-out), stride-aware blits/text/DIB/D3D9/BMP, reused wgpu upload scratch | ✅ — region packs pad to COPY width, not source pitch |
| 1c. D3D9 wake coalescing + `WIE_PRESENT_PACING_HZ` pacing knob | ✅ — wake gate in the channel; pacing sleep in `d3d9/device.rs::present_pacing_wait` |
| 1d. GDI DIB host_slice zero-copy fast path | ✅ — whole-DIB borrow + one-row fallback (`gdi32/dib.rs`); note: `dib_row_offset` returns a ROW index — callers scale by the byte stride |

## Wave 2 — command capture + render thread (in progress)

| Item | Status |
| --- | --- |
| Capture GDI/D3D9 draw calls into a command stream instead of rasterizing inside WinAPI handlers | 🟡 **D3D9 slice landed 2026-09-05** — `d3d9/capture.rs` op stream + `present/stream.rs` capture render thread (`wie-d3d9-capture`): `Draw*`/`Clear` handlers record self-contained ops (per-draw device-state snapshots: matrices/viewport/render state/stages/textures/shaders/constants; guest bytes read at record time) and `Present` flushes the stream to the render thread, which replays into its OWN backbuffer + RT/depth copies and publishes (commit-thread publish path reused). RT/depth round-trip: `Arc`'d target texels, flush-input/handback protocol with sync points at RT lock/unlock (`emu_dirty` + `*_seen` sets; details in the `d3d9/capture.rs` module doc). Opt-in `WIE_CAPTURE_STREAM=1` (GUI host only; headless/CI keep the legacy inline raster path — the shared `rasterize_frame` core keeps both byte-identical). Hash proof: `micro_gui_window::capture` pins `D3D9_RESTING_FRAME_HASH`. Flip default-on after the gate passes. GDI stays inline (post-Wave-1 it is memcpy-level) |
| Dedicated render thread consumes the stream; emu thread never rasterizes | 🟡 **slice 1 landed 2026-09-05** — the A1 skeleton: `present/commit.rs` commit slot + `wie-present-commit` render thread; `IDirect3DDevice9::Present` hands the finished backbuffer to the committer (pointer move, spare-pool recycled) and the stretch + publish run off the big lock (GUI sessions only; `WIE_PRESENT_COMMIT=0` opts out; headless keeps the legacy path — CI hashes unchanged). Hash-equivalence proof: `micro_gui_window::commit` runs gui_d3d9 under commit mode and pins `D3D9_RESTING_FRAME_HASH`. Profile: `commit_frames/commit_ms` counters. GDI stays inline (post-Wave-1 it is memcpy-level); wgl publishes inline too |
| Kill the remaining big-lock reads from the host (z-order, window list already mirrored; audit leftovers) | 🟡 **landed 2026-09-05** — presenter-side `WindowMirror` on `PresentChannel` (geometry/parent/visibility/title/menu/mouse-tracking projection + focus/capture handles + `menu_dirty`). Guest-side mutation sites bump `WindowState.window_mirror_rev` (centrally in `find_window_mut`, plus focus/capture/menu/setters and create/destroy); `HandlerContext::finish` syncs the projection when the rev moved — clean cost is two integer compares. Host reads now mirror-based (no big lock): `window_at`, `window_at_in`, `capture_target`, `mouse_tracking`, `first_guest_window_handle/info`, `focused_top_level`, `window_menu_items` fast path (cache-hit without the lock; rebuild path stays locked), `set_key_state` (pushes `(vk, pressed)` events the guest keyboard readers drain under the lock they already hold). `resize_window` keeps `try_lock` + explicit re-sync (mutates outside any handler). Legitimately still big-lock (documented, non-hot): `focus_window`, `edit_selection`, `control_text`, `status_bar_part_text` (test-only observation), `take_host_geometry_request` (consumes a slot), `present_publish_ns_last` (diagnostics). Non-blocking proofs: `session::window::tests` holds the big lock and probes every mirrored accessor on a worker thread with a hard timeout |
| Acceptance: 40 s profile shows guest >90% CPU while a paint-heavy app renders | 🟡 **harness ready 2026-09-05** — `scripts/acceptance-wave2.sh` runs the 40 s capture on gui_d3d9's continuous-present mode and checks the invariants; the developer-local RUN (release build, idle machine) + `WAVE2_BASELINE=1` commit is the remaining human step |

## Wave 3 — JIT throughput (ADR-0002, the accepted-but-unbuilt fix)

| Item | Status |
| --- | --- |
| Feasibility pass: map x19–x28 = rax…r15 fixed order, `x14` rflags carrier; enumerate spill sites in `jit/lower/*` + `engine.rs` trampoline | 🟡 **done 2026-09-05** — spill sites enumerated: dispatcher entry mass-copy (`pipeline.rs run_compiled`), block-entry live-GPR/rflags loads (`lower/mod.rs compile_block`), **per-chain-edge `writeback_gprs` + successor reload (`lower/emit.rs emit_chain_or_exit` — the hot tax)**, block-exit stores, dispatcher exit writeback; Rust-side trampoline twins (`mark_dirty`/`chain_tail`). ABI constraint: literal x19–x28 residency is inexpressible via Cranelift under the aarch64 C ABI (17 results > register window, no custom call-conv) → staged plan: tail-call chaining → hand-written prologue stub (trampolines.rs pattern) for the reg map → SSA rflags (details in ADR-0002 status) |
| Direct block→block chaining (`b`/`br`, regs live) with dispatcher fallback (miss/SMC/fake-VA) | 🟡 **slice 1 landed 2026-09-05** — tail-call chain hops: `return_call`/`return_call_indirect` in `emit_chain_or_exit` (successor reuses the caller frame; no prologue/epilogue/ret per hop), `WIE_JIT_TAILCHAIN=0` restores the nested-call hop. `MAX_CHAIN_DEPTH` deliberately kept as the periodic dispatcher bounce (stop/Ctrl+C/hook checks run only in the Rust pump). regs-live hand-off still awaits the prologue-stub ABI (row above); dispatcher fallback/IC/chain-table untouched |
| SSA rflags (`zf/sf/cf/of` per ALU) so `test; je` consumes ZF without pack/unpack | ⬜ |
| Opt-out `WIE_JIT_DIRECT_REGS=0` restores the `JitCtx` path; oracle diff vs `WIE_CPU=iced` | ⬜ |
| Multi-module compile (split the single `JitShared::engine` Mutex) | ⬜ after direct regs |
| Target: `long_loop` ~0.25 s → ≤0.10 s | ⬜ measure |

## Wave 4 — degrade-not-die ISA coverage

| Item | Status |
| --- | --- |
| Unimplemented mnemonic → per-instruction fallback (interpreter stub returning partial state), not session stop (`exec/mod.rs:656` today) | ⬜ |
| x87 subset (fld/fmul/fadd/fstp/comparisons) — most common game FP path | ⬜ |
| Integer SSE via existing scalar helpers widened to packed forms | ⬜ |
| Coverage metric: % of micro-suite instructions executed without stop | ⬜ |

## Wave 5 — audio / input / threading fidelity

| Item | Status |
| --- | --- |
| winmm/waveOut ring buffer with a real audio callback thread | ⬜ |
| DirectInput/RawInput pass-through; winit event → guest queue without big lock | ⬜ |
| Multi-queue message pump per thread (`user32/message` today is single-queue) | ⬜ |
| Timer resolution independent of the message pump | ⬜ |

## Wave 6 — GPU offload (only if Wave 2+3 leave headroom on the table)

| Item | Status |
| --- | --- |
| Translate D3D9 fixed-function/ShaderModel draw streams to wgpu pipelines | ⬜ |
| Render-target texture pool keyed by (w,h,format) | ⬜ |
| Decision gate: re-run the FPS harness; offload only if software raster still dominates a frame budget | ⬜ |
