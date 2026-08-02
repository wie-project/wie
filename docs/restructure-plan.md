# Restructure Plan: SoC + KISS, no 3000-line files

Status: Proposed. Applies to all five crates (`wie-pe` → `wie-cpu` → `wie-winapi` → `wie-runtime` → `wie-cli`).

## Context

The workspace is 102,352 lines. Eight source files exceed 3000 lines, and a further ~20 sit between 1000 and 3000. The giants are not dead weight: they are the emulator's core (JIT lowerer, iced interpreter, winapi state/dispatch registry, D3D9 COM surface, runtime session loop). The restructure must therefore be **behavior-preserving and performance-preserving**: the JIT hot path, the dense `WinApiId` dispatch (no string compare), and the publish pipeline are invariants. Any change that regresses `long_loop` (0.28–0.32 s release JIT) or the micro-suite is a failure.

Current census (top offenders, lines):

| File | Lines | Cluster |
| --- | --- | --- |
| `wie-cpu/src/jit/lower.rs` | 8085 | JIT lowering |
| `wie-winapi/src/lib.rs` | 5822 | WinAPI state + re-export aggregator |
| `wie-cpu/src/exec.rs` | 4088 | iced interpreter |
| `wie-winapi/src/d3d9.rs` | 3773 | D3D9 COM surface |
| `wie-cpu/src/jit/mod.rs` | 3704 | JIT pipeline |
| `wie-runtime/src/session.rs` | 3260 | Runtime monolith |
| `wie-winapi/src/kernel32/file_io.rs` | 3250 | File I/O |
| `wie-winapi/src/d3d9_render.rs` | 3068 | Software rasterizer |
| `wie-winapi/src/dispatch_table.rs` | 2855 | WinApiId registry + dispatch |
| `wie-winapi/src/ucrt.rs` | 2342 | UCRT |
| `wie-cpu/src/mem/mod.rs` | 2253 | Guest memory |
| `wie-winapi/src/user32/window.rs` | 2163 | Window lifecycle |
| `wie-winapi/src/user32/message.rs` | 1695 | Message pump |
| `wie-winapi/src/user32/controls.rs` | 1667 | Control state machine |
| `wie-winapi/src/kernel32/mod.rs` | 1658 | kernel32 aggregator |
| `wie-winapi/src/gdi32/state.rs` | 1557 | GDI object state |
| `wie-runtime/src/guest_stubs.rs` | 1507 | In-guest stub tables |
| `wie-cpu/src/jit/block.rs` | 1291 | Block build + lowerable predicates |
| `wie-winapi/src/kernel32/misc.rs` | 1269 | kernel32 misc |
| `wie-pe/src/lib.rs` | 1221 | PE parse/load |
| `wie-winapi/src/exception.rs` | 1137 | Unwind engine |
| `wie-winapi/src/pthread/locks.rs` | 1131 | pthread sync |
| `wie-winapi/src/seh.rs` | 1091 | SEH dispatcher |
| `wie-winapi/src/user32/dialog.rs` | 1073 | Modal dialogs |
| `wie-winapi/src/kernel32/console.rs` | 1014 | Console |

All other files are <1000 lines. Tests: `micro_gui_window.rs` (933) is the largest test and stays — it is linear integration, not a module.

## Options considered

| Option | Move | Trade-off |
| --- | --- | --- |
| A — split only the 8 files >3000 | Meets the letter of the requirement | Leaves ~20 monoliths of 1000–2900 lines; weakest SoC outcome |
| **B — split everything >~1200 along mapped seams** | Every module becomes one coherent concern; ~10 files stay 800–1200 | Most commits; touches cpu crate where perf risk is highest |
| C — aggressive (also dedupe helpers, new shared utils, redesign boundaries) | Cleanest end state | Violates KISS; high churn; unverifiable in one pass |

**Recommendation: B**, executed as pure structural moves. Each split follows a verified seam; no new traits, no new abstractions, no cross-cutting rewrites. Reversibility: every step is a file move + `impl`-block relocation, so any phase can be reverted independently.

## File-size policy (new, enforced)

- Hard cap: **1500 lines** per source file (headroom below the 3000 ask; files ≥1500 must be split).
- Target: **≤1000 lines** per file.
- Exception: pure data tables (e.g. `WINAPI_NAME_ROWS`, D3D9 vtable slot tables) may live in dedicated data files of any length.
- Enforcement: `scripts/check-file-sizes.sh` (find + `wc -l` + awk) wired into `scripts/check.sh`; policy note added to `CONTRIBUTING.md`.

## Target architecture

### wie-winapi

`lib.rs` shrinks to a module tree + re-export aggregator (~300 lines). All state types move to `state/` (domain-split), keeping the `pub use` surface byte-identical so `wie-runtime`/`wie-cli` compile untouched.

- `state/mod.rs` — `WinApiState`, `KernelState`, `DllStateMap`, `HandlerContext`
- `state/window.rs` — `WindowState`, `WindowRecord`, `WindowClassRecord`, `QueuedWindowMessage`, `TimerRecord`, hooks, atoms
- `state/process.rs` — `ProcessState`, `HeapState`, `FileIoState`, env, registry
- `state/d3d9.rs` — `D3D9State`
- `state/input.rs` — `KeyboardState`
- `state/files.rs` — `OpenGuestFile`, `VirtualGuestFile`, `FindHandle`, mounts
- `state/misc.rs` — remaining records (FlsSlot, RegistryKey, HeapAllocation, enums)

Per-DLL files become directories with `mod.rs` re-exporting the split items; the crate-root `pub mod` declarations are unchanged (a `mod d3d9;` resolves to `d3d9/mod.rs` equally), so **no lib.rs edits are needed for directory conversions**.

- `dispatch_table/` — `mod.rs` (enum + traits + top-level range match), `names.rs` (per-DLL `WINAPI_NAME_ROWS` const arrays merged with `concat!`), `kernel32.rs`, `user32.rs`, `gdi32.rs`, `d3d9.rs`, `ucrt.rs` (+ remaining DLLs) holding per-DLL dispatch arms. Hot path preserved: top-level match on `u16` id, per-DLL arm still one dense match; no string compare added.
- `d3d9/` — `mod.rs` (create/device/vtable dispatch), `shader.rs` (P5a shader objects), `raster.rs` (FFP draw ~1698–2300), `texture.rs` (P4b lock/unlock ~2792–3100), `blend.rs` (P4c blend/depth ~3592–3773).
- `d3d9_render/` — `mod.rs` (state structs + `rasterize_triangle`), `vertex.rs` (FVF parse/transform ~507–759), `ps.rs` (PS 2.0 interpreter ~779–1332), `sample.rs` (texel/sampling ~1332–1492), `blend.rs` (depth/blend/edges ~1492–1656). Tests move out of the giant into `mod tests` in `mod.rs`.
- `kernel32/file_io/` — `mod.rs` (handlers + read/write), `open.rs` (open table + `open_guest_path` family ~447–820), `path.rs` (full/short/long path ~2115–2560), `dir.rs` (find/attr/create/delete/move ~57–205 + 2773–2875), `time.rs` (file times ~871–1027).
- `kernel32/misc/` — `mod.rs` (version/clock/FLS/error ~1–381), `time.rs` (~410–580), `identity.rs` (~715–938); SEH handlers (~1034–1269) move to `seh.rs`'s module.
- `kernel32/mod.rs` — keep constants/result type/helpers; move file-I/O handlers out to `file_io/`.
- `ucrt/` — `mod.rs` (dispatch table), `stdio.rs`, `crt.rs` (init/args/env/exit), `string.rs` (mem/string/ctype/wide), `misc.rs` (time/rand/EH/thread/locale).
- `user32/window/` — `mod.rs` (lifecycle ~1–700), `geom.rs` (geometry/DPI ~700–1300), `class.rs` (class long ptrs ~2000–2163).
- `user32/message/` — `mod.rs` (pump core: Peek/Get/Post/Send/Translate/Dispatch ~454–1410), `class.rs` (Register/UnregisterClass ~1420–1649), `synth.rs` (message synthesis ~113–351).
- `user32/controls/` — `mod.rs` (dispatch + enums + `deliver_command`), `button.rs`, `edit.rs` (~1064–1364), `listbox.rs` (~1320–1483), `paint.rs` (shared paint helpers).
- `gdi32/state/` — `mod.rs` (ops: GetObject/SelectObject/DC ~30–460), `objects.rs` (brush/pen/font/dib creation ~460–843), `metrics.rs` (text metrics + colors ~876–1122), `records.rs` (types ~1133–1440).
- `guest_stubs/` — `mod.rs` (dispatch), per-table files (clock, dialog, heap ctrl, metrics/colors).
- `exception/` — `mod.rs` (structs + lookup ~1–260), `unwind.rs` (`virtual_unwind` engine ~378–1137), `dwarf.rs` (`dw_eh_pe` ~335).
- `pthread/locks/` — `mod.rs` (dispatch + mutex), `cond.rs` (~500–800). `pthread/threads.rs` (981) stays.

Keep as-is (<1500, cohesive): `seh.rs` (1091), `user32/dialog.rs` (1073), `console.rs` (1014), `console_cells.rs` (1122), `sync.rs` (924), `text.rs` (972), `blit.rs` (869), `fake_va.rs` (970), `dll_loader.rs` (899), `d3d9_shader.rs` (887), `threads.rs` (981), `resources.rs` (991).

### wie-runtime

- `session/` — `mod.rs` (RuntimeSession struct + entry points), `init.rs` (PE load + `register_layout_regions` ~312–542, 714–1300), `pump.rs` (`run_until_stop` + drain ~1509–2402), `window.rs` (GuestHandle + window tree/hit-test ~2727–3081), `menu.rs` (MenuNode/build_menu_tree ~3091–3260), `profile.rs` (RuntimeProfile ~125–305), `callback.rs` (PendingGuestCallback ~108–121, 2582–2648).
- `guest_stubs.rs` (1507) → `guest_stubs/` split by table family.
- Tests stay in `tests/`; `micro_gui_window.rs` (933) untouched.

### wie-cpu (highest perf risk — gate every phase with timings)

- `jit/lower/` — `mod.rs` (JitCtx + orchestration ~1407–2006), `tlb.rs` (~32–942), `analysis.rs` (liveness ~2006–2680), `sse.rs` (mov/pack/binop ~2688–3650), `string.rs` (~5491–5726), `gpr.rs` (operands + ALU ~5800–6291, 6898–7600), `emit.rs` (IR emission ~6291–6577), `mem.rs` (guest-memory helpers ~6577–6898), `flags.rs` (~7786–8085). Implementation note: helpers that are methods on `JitCtx` keep working via `impl super::JitCtx` blocks in submodule files; `#[inline]` and `unsafe` annotations move verbatim.
- `exec/` — `mod.rs` (step + `execute_one` ~161–667), `cache.rs` (decode cache ~52–245), `gpr.rs` (~764–1933), `sse.rs` (exec + type enums + JIT-facing helpers ~1281–2643), `string.rs` (REP + host bridge ~2803+).
- `jit/` — `mod.rs` shrinks to `JitCpu` + engine; `config.rs` (knobs ~62–236), `shared.rs` (`JitShared` + bg worker ~244–748), `pipeline.rs` (compilation ~1002–1860), `cpu_engine.rs` (`impl CpuEngine` ~2265–2748).
- `mem/` — split `GuestMemory::impl` (~60 methods, 137–1493) into `map.rs`, `alloc.rs`, `query.rs` via `impl super::GuestMemory` blocks.
- `jit/block.rs` (1291) — optional: `lowerable.rs` (predicates ~218–875), `stackpin.rs` (~904–1260). Deferred unless it stays >1500.

### wie-pe

- `lib.rs` (1221) — optional split into `parse.rs`/`map.rs`/`identity.rs`; deferred (below cap).

## Execution phases (each = one or more commits, gated)

1. **Phase 0 — baseline**: full gate (`./scripts/check.sh` + micro-suite + `long_loop` timing recorded). No code change.
2. **Phase 1 — winapi `lib.rs` state extraction** (serial; lib.rs is shared): move state types to `state/`, keep `pub use` surface identical. Gate: build + clippy + workspace tests.
3. **Phase 2 — winapi module dirs** (parallel lanes, disjoint ownership):
   - L2a: `dispatch_table/` (names + per-DLL arms)
   - L2b: `d3d9/` + `d3d9_render/`
   - L2c: `kernel32/` (file_io/, misc/)
   - L2d: `ucrt/`
   - L2e: `user32/` (window/, message/, controls/)
   - L2f: `gdi32/state/` + `exception/` + `pthread/locks/` + `guest_stubs/`
   Gate: full workspace gate + micro-suite (gui_exes, dll_tests) + `WIE_RUNTIME_PROFILE` sanity.
4. **Phase 3 — runtime `session/`** (single lane): splits above. Gate: workspace + micro-suite + `gui_demo` interactive sanity.
5. **Phase 4 — wie-cpu** (serial lanes, perf-gated after each): `exec/` → `jit/` (config/shared/pipeline) → `jit/lower/` → `mem/` impl-split. Gate per lane: build + clippy + cpu tests + `long_loop` timing; full gate + `check-jit-matrix.sh` after all.
6. **Phase 5 — guard + docs**: `scripts/check-file-sizes.sh` wired into `check.sh`; CONTRIBUTING.md size policy; README architecture notes updated.
7. **Phase 6 — final gate**: full `check.sh` + micro-suite + timings; all green.

## Verification (per phase)

- `cargo fmt --all` + `cargo clippy --workspace --all-targets -- -D warnings` (lint policy unchanged)
- `cargo test --workspace` (507 tests must stay green)
- `make -C micro-exes && ./scripts/run-micro-suite.sh` for phases touching winapi/runtime
- `long_loop` ≈0.28–0.32 s (release JIT) for any wie-cpu phase; `check-jit-matrix.sh` once after Phase 4
- No new `#[allow]`, no new `unsafe`, no new `pub` surface (all splits stay `pub(crate)` where the original was)

## ADR-001: Split by cohesion, not by line count

- **Status:** Accepted (with this plan)
- **Context:** A line-count cap alone invites arbitrary cuts. Files above the cap were inspected and every split follows an existing seam (function family, lifecycle stage, DLL domain, data vs logic).
- **Decision:** Structural moves only; no behavior change, no new abstractions. `WinApiId` enum and dense dispatch match stay single blocks; data tables move to data files.
- **Consequences:** Easier: review, navigation, parallel lanes, future growth stays under the cap. Harder: commit history interleaves moves and logic touches only when a seam requires visibility changes (`pub(crate)`), which the lint gate catches.

## ADR-002: File-size policy 1500/1000

- **Status:** Accepted
- **Context:** User requirement: no 3000-line files. 1500 hard / 1000 target gives margin, keeps modules small enough to read in one sitting, and exempts pure data tables.
- **Decision:** Policy above, enforced by `check-file-sizes.sh` in `check.sh`.
- **Consequences:** Easier: enforced SoC going forward. Harder: a few files that would naturally grow (dispatch tables) must stay data-only or be split by DLL — acceptable, and DLL-split is already the design.
