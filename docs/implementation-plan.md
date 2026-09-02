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
| End-to-end FPS harness (release build, headless windowed run, frames/s + present/jit counters, saved under `docs/baselines/`) | ⬜ pending — wire into `.github/workflows/nightly-bench.yml` |
| Full workspace + micro suite green after each wave | ⬜ re-verify per wave |

## Wave 1 — present-domain decoupling (done 2026-09-02)

| Item | Status |
| --- | --- |
| 1a. `PresentChannel`: latest-wins slot, wake gate (`published_seq`/`taken_seq`), presenter reads without the big lock | ✅ — `wie-winapi/src/present/mod.rs`, `GuestHandle` wired; hand-back now zero-ALLOC via the channel's spare pool (displaced slots + presented buffers); `pending_frames()` re-arm wired into `RedrawRequested` |
| 1b. ADR-0001: 64-px pitch padding (`padded_stride`, `WIE_SURFACE_PAD=0` opt-out), stride-aware blits/text/DIB/D3D9/BMP, reused wgpu upload scratch | ✅ — region packs pad to COPY width, not source pitch |
| 1c. D3D9 wake coalescing + `WIE_PRESENT_PACING_HZ` pacing knob | ✅ — wake gate in the channel; pacing sleep in `d3d9/device.rs::present_pacing_wait` |
| 1d. GDI DIB host_slice zero-copy fast path | ✅ — whole-DIB borrow + one-row fallback (`gdi32/dib.rs`); note: `dib_row_offset` returns a ROW index — callers scale by the byte stride |

## Wave 2 — command capture + render thread (next)

| Item | Status |
| --- | --- |
| Capture GDI/D3D9 draw calls into a command stream instead of rasterizing inside WinAPI handlers | ⬜ |
| Dedicated render thread consumes the stream; emu thread never rasterizes | ⬜ |
| Kill the remaining big-lock reads from the host (z-order, window list already mirrored; audit leftovers) | ⬜ |
| Acceptance: 40 s profile shows guest >90% CPU while a paint-heavy app renders | ⬜ |

## Wave 3 — JIT throughput (ADR-0002, the accepted-but-unbuilt fix)

| Item | Status |
| --- | --- |
| Feasibility pass: map x19–x28 = rax…r15 fixed order, `x14` rflags carrier; enumerate spill sites in `jit/lower/*` + `engine.rs` trampoline | ⬜ |
| Direct block→block chaining (`b`/`br`, regs live) with dispatcher fallback (miss/SMC/fake-VA) | ⬜ |
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
