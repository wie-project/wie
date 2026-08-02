# GPU / Metal Support Plan

Status: proposed — awaiting review (Aug 1, 2026).
Companion: `docs/gui-design.md`; live progress in `.slim/deepwork/gui-implementation.md`.

## Principles (unchanged, hard constraints)

1. **Zero `unsafe` outside wie-cpu.** The repo denies `unsafe_code` workspace-wide except the Cranelift JIT entry points. Every prior decision followed this (muda over raw AppKit for menus; ab_glyph+fontdb over CoreText for fonts). The GPU path must respect it too — this rules out direct `objc2-metal` calls in wie-cli.
2. **Software correctness oracle first.** The CPU rasterizer is the reference; GPU work only where it beats it and keeps determinism where the test suite needs it.
3. **Deterministic micro-suite.** GDI hash gates (`GUI_BLIT_RESTING_FRAME_HASH`) stay CPU-rendered. D3D9 pixel tests stay CPU-rendered; GPU paths get behavior tests, not pixel hashes (GPU output varies by device/driver).
4. **macOS-only.** Metal is the only GPU API we care about.

## Decision 1 — Present layer: wgpu (Metal backend) — LOCKED (user decision, Aug 1)

**Use wgpu for all GPU work. No objc2-metal.** (User: "use wgpu for the rendering not objc2-metal".) This keeps the zero-unsafe policy intact and gives one dependency for present + D3D9 slice-2 GPU future.

### Why wgpu

- Fixes the two present-path problems from the resize investigation in one move: softbuffer's macOS backend allocates a **fresh zeroed buffer on every `buffer_mut()`** (so B3 regions cannot be used for partial copies), and every frame does a full `copy_from_slice` (~4 MB at 1280×800, scales with window size).
- With wgpu we own the texture: `Surface` backed by CAMetalLayer, one `Rgba8UnormSrgb` texture, per-frame `queue.write_texture` of **only the dirty region** (`SurfaceFrame.region`), then a blit/composite to the surface. B3 regions become real upload savings instead of bookkeeping.
- The `Arc<Vec<u32>>` publish hand-back (B1) still feeds the upload; no change to the guest-side pipeline.
- One dependency for present AND the D3D9 slice-2 GPU future — a second GPU crate would be redundant.

### Rejected

- **Scoped `unsafe` objc2-metal module in wie-cli** — rejected by user decision: breaks the zero-unsafe policy; direct CAMetalLayer control is not worth it when wgpu covers present + compute offload safely.
- **Keep softbuffer + persistent host buffer** — rejected: the fresh-buffer problem stays; the present redesign's core win ("kill the per-frame full copy") is not achieved.

### What changes

- `crates/wie-cli/src/gui/app.rs`: `softbuffer::Surface` → wgpu surface + texture; `RedrawRequested` uploads `frame.region` (or full) via `write_texture`; `surface.resize` on `Resized`.
- Keep `WIE_PRESENT=softbuffer` as a kill-switch fallback (RUNBOOK style) while wgpu proves out; flip default later.
- Present still happens on the winit main thread (wgpu surfaces are main-thread-only, same as now).
- Threading stays decoupled: guest thread publishes frames; host event loop uploads.

### P4a design (locked Aug 1, after wgpu research — lib-2)

- **wgpu 30.0.0**, `wgpu = { version = "30", default-features = false, features = ["metal"] }` in wie-cli; surface created from `&Window` directly (winit 0.30 rwh_06 implements `HasWindowHandle` — no unsafe, no Arc needed).
- **Alpha correction (research gap)**: the research's "write_texture directly into the surface texture + present, no render pass" path is WRONG for WIE — our frame bytes are `0RGB` with the alpha byte always 0 (softbuffer's cg backend masked it via `CGImageAlphaInfo::NoneSkipFirst`; wgpu honors alpha). A direct upload would present a fully transparent window. Therefore:
  - One **persistent staging texture** (`Rgba8Unorm`, `COPY_DST | TEXTURE_BINDING`, sized to the guest frame; recreated only on frame-size change).
  - Per frame: `write_texture(region | full)` into staging (`bytes_per_row = frame_width * 4` — full row pitch even for sub-rects, per lib-2), then one **blit render pass** (fullscreen triangle, trivial WGSL) sampling staging nearest and writing `vec4f(rgb, 1.0)` into the surface texture, then `present()`.
  - This makes B3 region uploads REAL (staging persists, unlike softbuffer's fresh-buffer-per-call) and gives the alpha=1 guarantee.
- **Color fidelity**: prefer `Bgra8Unorm` (non-sRGB) from surface capabilities — guest pixels are already sRGB-encoded; a non-sRGB surface avoids double conversion and keeps pixels byte-identical to the softbuffer path (CI-hash philosophy). Fallback: `caps.formats[0]`.
- **Present mode**: `Fifo` (vsync, matches softbuffer's blocking present).
- **Resize / drag-stretch**: staging stays frame-sized; the blit pass nearest-scales to the window-sized surface — replaces the CPU `stretch_nearest` in the wgpu path with the GPU sampler (identical nearest semantics).
- **Async**: `pollster::block_on` for adapter/device requests (no runtime in wie-cli).
- **Bytes**: `wgpu::util::cast_slice(&frame.pixels)` (safe) for the upload — no new dep for u32→u8.
- **Error handling**: repo lints deny unwrap/panic — map wgpu errors with anyhow; `Timeout`/`Occluded` skip the frame, `Outdated` reconfigure, `Lost` recreate surface + configure. On wgpu init failure, log + fall back to softbuffer (env `WIE_PRESENT` selects backend, default wgpu).
- **B2 skip preserved**: generation+size skip stays in app.rs before present (identical frame bytes; hash gates unaffected — they run on published frame bytes, not the present path).

### P4a status: DONE (fix-38, Aug 1) — wgpu-30 API deviations from the spec above

Verified: release build clean, clippy 0, fmt clean, workspace green, `GUI_BLIT_RESTING_FRAME_HASH` unchanged, `WIE_PRESENT=wgpu|softbuffer` smoke both stay open with region publishes. wgpu 30 renamed/changed vs the plan:

1. `wgpu::util::cast_slice` gone → `bytemuck::cast_slice` (added `bytemuck = "1"`).
2. `SurfaceTexture::present()` removed → `Queue::present(&self, stex)`.
3. `ShaderSource::Wgsl` behind the wgpu `wgsl` feature (not implied by `metal`) → added it.
4. `SurfaceConfiguration` gained required `color_space: SurfaceColorSpace` → `Auto`.
5. `get_current_texture()` returns the plain `CurrentSurfaceTexture` enum (`Success/Suboptimal/Timeout/Occluded/Outdated/Lost/Validation`), not a `Result`.
6. New required fields: `DeviceDescriptor.{experimental_features, memory_hints, trace}`, `RenderPipelineDescriptor.{multiview_mask, cache}`, `PipelineLayoutDescriptor.{bind_group_layouts: &[Option<_>], immediate_size}`.
7. `pollster` latest = 1.0 (plan said 0.4).
8. `create_surface` from `Arc<Window>` (rwh_06 blanket impl) yields a `'static` surface; presenter owns a window clone (no self-referential struct).

Known unrelated flake at P4a time: `gui_dialog_shift_tab_moves_focus` fails ~50% in the full `micro_gui_window` suite (passes in isolation and in `cargo test --workspace`); verified pre-existing (fails identically with P4a stashed). Hardening lane pending after P4b to avoid test-file write conflicts.

## Decision 2 — D3D9 slice 2 (textures, blend, depth): CPU-first in the software rasterizer

Extend `crates/wie-winapi/src/d3d9_render.rs` (slice 1: Clear/BeginScene/EndScene/Present/DrawPrimitiveUP with Gouraud vertex colors).

Scope per render-state family (all D3D9 FFP semantics):

1. **Textures**: `CreateTexture` / `GetSurfaceLevel` / `LockRect` / `UnlockRect` / `SetTexture(0..7)` / `SetTextureStageState` — RGBA8888 texture cache in `D3D9State`; nearest + linear sampling in the rasterizer's fragment stage; texture-stage color ops (`D3DTSS_COLOROP` MODULATE/SELECTARG1/etc.) for the common subset.
2. **Blend**: `SetRenderState(D3DRS_ALPHABLENDENABLE | SRCBLEND | DESTBLEND)` — per-fragment alpha blend in the rasterizer.
3. **Depth**: `CreateDepthStencilSurface` / `SetDepthStencilSurface` / `D3DRS_ZENABLE | ZFUNC | ZWRITEENABLE` — depth buffer + test in the rasterizer.

### Why CPU-first

- Deterministic, hash-testable incrementally (extend `gui_d3d9` micro-exe: textured quad, blended quad, depth occlusion — CPU path can be pixel-verified).
- No async pipeline complexity; matches how every other WIE subsystem shipped (correctness first, speed second).
- The NEON rasterizer (P5) accelerates the same fragment stage; texture sampling is the classic SIMD win.

## Decision 3 — P5: shaders, NEON rasterizer, quarter-scale, D3D9→Metal research

- **PS 2.0 / VS interpreter**: constant tables, temp registers, texture fetches, the ~68-instruction D3D9 shader set — CPU interpretation in the fragment/vertex stage (correctness oracle).
- **NEON rasterizer**: vectorized fill + texture sampling in `wie-cpu/src/simd.rs` style.
- **Quarter-scale**: render D3D9 at 1/4 resolution, nearest-upscale in the wgpu blit (matches the roadmap's "P5 quarter-scale" intent; the upscale lives in the present shader).
- **D3D9→Metal research gate** (explicitly research, not commitment): measure the CPU rasterizer at quarter-scale on real games first. Only if CPU misses the frame budget do we evaluate offloading triangle rasterization to **Metal compute via wgpu** (same dependency, no new unsafe) — a software-style rasterizer in a compute shader, not a full shader-translation to Metal graphics.

## Milestones / lanes

| # | Lane | Deliverable | Rough size |
|---|---|---|---|
| P4a | wgpu present path (with softbuffer kill-switch) | region-based upload, resize handled | 1 lane |
| P4b | D3D9 textures (CreateTexture/LockRect/SetTexture + sampling + color ops) | textured quads render | 1–2 lanes |
| P4c | D3D9 blend + depth states | blended/depth-tested quads render | 1 lane |
| P5a | PS 2.0/VS interpreter | shader-based triangles render (CPU) | 2–3 lanes |
| P5b | NEON rasterizer (fill + sampling) | rasterizer speedup | 1 lane |
| P5c | Quarter-scale + wgpu upscale blit | D3D9 at 1/4 res | 1 lane |
| P5d | D3D9→Metal research gate | measured decision | research |

Order matters: P4a first (present is the bottleneck for everything on screen), then P4b→P4c, then P5 in sequence. P4b–P5c all extend the same `d3d9_render.rs` fragment stage, so they are strictly sequential (one writer).

## Test strategy

- `gui_d3d9` micro-exe extended per milestone: textured quad (nearest + linear), alpha-blended quad, depth-occlusion test (near quad occludes far quad). CPU path pixel-hashable; add a D3D9 resting-frame hash once P4b lands.
- Present path: existing 7 micro-gui tests must stay green with wgpu present (they exercise `take_frame`/publish, not softbuffer directly; the interactive hash gate stays CPU-side).
- `WIE_PRESENT=softbuffer` regression run in `scripts/check.sh`-style matrix.
- GDI hash gates unchanged (P4a must not perturb the GDI frame bytes — it only changes how bytes reach the screen).

## Out of scope (still)

- OpenGL / Direct2D / DirectWrite / GDI+ dispatch surfaces (no modules; revisit only if a target app needs them).
- Full D3D9 fixed-function pipeline beyond the common subset (indexed buffers already exist in slice 1; vertex streams/FVF beyond position+color+uv deferred to P5).

## Immediate follow-up from the roadmap cleanup

- `README.md` (lines 20, 223–236), `CLAUDE.md` (line 77), `docs/RUNBOOK.md` (docs map) still link the removed `Optimization ROADMAP.md` / the old roadmap docs. Fix references; the live roadmap is now `.slim/deepwork/gui-implementation.md`.

## P5b status: DONE (implemented directly by orchestrator, Aug 2, 2026)

NEON kernels `fill_0rgb_4x` / `blend_0rgb_4x` / `mul_0rgb_4x` in wie-cpu/src/simd.rs (aarch64 + scalar fallback, reference-equality tests), wired into d3d9_render.rs `rasterize_triangle` as `fast_blend` (SRCALPHA/INVSRCALPHA/ADD, 4-contiguous-pixel batches) and `flat_fill` (constant vertex color skips per-pixel Gouraud). Byte-identical proven by both resting-frame hash gates; workspace 503, clippy/fmt clean.

Honest perf: `blend_0rgb_4x` ≈ parity vs scalar in isolation (fair benchmark); the real win is `flat_fill` eliminating per-pixel `blend_colors` for constant-color triangles. Barycentric/edge f32 interpolation NOT vectorized (dominant cost for Gouraud geometry) — revisit if profiling demands.

Note: six delegated fixer lanes on this area returned empty reports with nothing in the tree (fix-43/44/45/47/48); the empty-report pattern is a delegation-mechanism issue, not a technical one. P5b was implemented directly by the orchestrator.

## P5c status: DONE (implemented directly by orchestrator, Aug 2, 2026)

`WIE_D3D9_SCALE=2|4|8` (default 1) divides the D3D9 backbuffer + viewport (d3d9.rs `init_device_state` + `d3d9_render_scale`). The guest renders into the quarter-size backbuffer (16× fewer fragments at scale 4 — the dominant rasterizer cost), and `handle_present`'s existing `bb != win` path `stretch_nearest`s to the window-sized published frame. The wgpu blit then uploads the full-size frame as before (CPU upscale in Present; the GPU-shader upscale variant is a future refinement — Present already produces the window-size frame).

Verified: default scale=1 byte-identical (micro_gui 7/7, both hash gates held); scale=4 → backbuffer 80×60, published frame still 320×240 (screenshot-confirmed), gui_d3d9 selftest exits 121 (its pixel checks hardcode full-res coordinates — expected). Knob documented in README.

## P5d status: IN PROGRESS — measurement lane (Aug 2, 2026)

## P5d status: DONE — research gate closed (measured, Aug 2, 2026)

Measurement (debug build, 200 frames of a full-screen 2-triangle quad through the real `draw_triangle` path, `WIE_D3D9_SCALE` knob):
- flat: 1280×800 = 0.301 ms/frame (3322 fps); 320×200 = 0.017 ms/frame (58942 fps) → ~18×
- blended (SRCALPHA/INVSRCALPHA): 1280×800 = 0.620 ms/frame (1613 fps); 320×200 = 0.054 ms/frame (18607 fps) → ~12×

**Decision: NO Metal-compute offload.** Even full-res blended in debug is 0.62 ms/frame — the 60 fps budget (16.7 ms) has ~25× headroom, and quarter-scale buys another ~12–18×. The software rasterizer is not a bottleneck; wgpu already handles present. Revisit Metal compute only if a real game workload ever exceeds the frame budget — and quarter-scale is the cheaper lever first.

## P5 series closeout (Aug 2, 2026)
- P5a-1 PS 2.0 core: DONE (earlier).
- P5a-2 VS interpreter + shader quad: PARKED (user decision; 6 delegated lanes returned empty reports — delegation-mechanism issue; direct implementation succeeded for P5b/P5c/P5d and is available for P5a-2 if unparked).
- P5b NEON rasterizer: DONE (direct implementation; byte-identical via hash gates).
- P5c quarter-scale: DONE (direct implementation; `WIE_D3D9_SCALE` knob).
- P5d Metal research gate: DONE (measured, decision recorded above).
