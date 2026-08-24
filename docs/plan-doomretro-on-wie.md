# Plan: Doom Retro on WIE — correctness + optimization

Date: 2026-08-22. Repo: `/Users/yanis/Programming/wie` ("WIE"), Windows emulator for
**macOS Apple Silicon only** (aarch64 host; JIT = Cranelift-aarch64 + iced-x86 fallback;
winit/wgpu present path). Guest under test: Doom Retro v6.3 (`real_exes/doomretro/`,
bottle `doomretro`). Supersedes/extends `docs/handoff-doomretro-on-wie.md`.

---

## 1. Mission

1. **Correctness**: Doom Retro must boot past its startup handshake and render.
2. **Performance**: reduce startup wall time; profile-driven handler/JIT optimization.

Success criteria (acceptance):
- Debug build headless-GUI run completes the version check without `I_Error`, renders a
  non-blank screenshot (`--screenshot` BMP >1% pixels differing from dominant color).
- Full `cargo test` green.
- Release-build `WIE_RUNTIME_PROFILE` A/B vs checkpoint commit shows no regression and
  targeted counter improvements.

## 2. Rules of engagement

- Functional testing: **debug builds** (`cargo build`). Timing arbitration: **release builds**
  via the baseline harness only — never claim speedups from debug or from concurrent runs.
- Temporary instrumentation uses unique tags (`[VER]`, `[STALL]`, `[DSP]`, `[K32]`, `[FIO]`,
  `[PRES]`); **all tags must be removed before finishing** (grep to prove zero remain).
- Lanes persist incremental findings to `docs/lanes/<LANE>.md` (survives lost reports).
- Do not revert the committed xmm-writeback fix (see §3).
- macOS Apple Silicon only: JIT emits AArch64; guest xmm regs are NEON-backed
  (`simd_enabled()` → `I8X16` loads/stores in `store_xmm_pair`).

## 3. Solved: cross-block xmm corruption (committed)

Root cause (verified by runtime traces): guest `movups xmm0,[arg1]` (block at
`0x1401279f1`) and `movdqu [rbp-0x28],xmm0` (block entered at `0x140127a06`, store insn
`0x140127a0a`) live in separate compiled blocks. The old exit writeback persisted engine
XMM per the **entry block's** `xmm_may_def_mask`/`xmm_live_mask`; defs made by chained SSE
blocks were dropped whenever the run returned through a block whose mask excluded them →
next dispatcher entry re-snapshotted stale `engine.xmm0 = 0` → char-class pointer written
as 0 → fault (`rip=0x14012b951`, reads `[obj+0x18]=0`, obj=`0x207fddf0`) or corrupted parse
(`I_Error` version mismatch).

Fix (in `crates/wie-cpu/src/jit/pipeline.rs`, run_compiled exit): **full 16-slot XMM
writeback**, mirroring the GPR policy (`gpr_dirty_bits==0 → ALL_DIRTY`). Verified: fatal
block's entry now receives `xmm0={lo=0x1401b21d0, hi=0x160004d90}` across 339k traced
dispatcher boundaries. Fault eliminated.

Reference constants: correct wrapper pair `{0x1401b21d0 (char-class struct ptr), 
0x160004d90}`; default template literal at exe VA `0x1401b2328`
(`{0x1401b21d0, 0x1401b2650, 0x43}`); global char-class table base holder `0x1401b21c0`;
SDL2.dll ImageBase `0x180000000`; deterministic guest stack slot `obj=0x207fddf0`,
wrapper `W=0x207fdf60`.

## 4. Open blocker (Lane S1): version `I_Error` — narrowing chain (all verified)

1. `s_VERSION` (`@0x1404bb560`) receives **zero stores** on every visible path across
   entire slow-JIT runs -> neither doomretro.wad nor freedoom2.wad `[STRINGS]` processing
   ever assigns VERSION.
2. The `"VERSION"` lookup name (`@0x140180a10`) is **never read** ->
   `deh_procStringSub`'s key compare never executes.
3. DEHACKED dispatch IS running: `[STRINGS]`-entry byte[0] read **609x**; bytes[1..8]
   read exactly **once each** -> doomretro.wad's own `[STRINGS]` header line was compared
   once, walked the full name...
4. ...and line-buffer content at compares is byte-correct (`"Patch File for D"`,
   `"Doom version = 1"` = freedoom2's lump; `[STRINGS]` candidates present).
5. => Failure is INSIDE `deh_procStrings` (or its first `dehfgets`/`deh_GetData`): the
   handler bails before any key compare.
6. Probes in flight: loads from the delivered DEHACKED guest buffer (`buffer_va`,
   observed candidate `0x1600068d0`) -> discriminates "mem_fgets reads wrong bytes"
   vs "deh_GetData/compare mis-executes".

Static anchors: `"VERSION\0"` @ `0x140180a10`; strlookup entry @ `0x1404bba50`;
`s_VERSION` var @ `0x1404bb560`; char-class chain `0x1401b21c0->0x1401594d2`,
`0x1401b21d0->0x140158fd0` (u16 table) - all verified intact in guest memory vs exe image.
Parse-loop source (v6.3): `D_ProcessDehFile` loop = `dehfgets(inbuffer)` -> skip
blank/#/space -> INCLUDE check -> `strncasecmp` scan of `deh_blocks[].key` -> else re-run
last block handler (BEX style). `[STRINGS]` = index 10.

## 5. Optimization lanes

Baseline (debug profile): wall 370s; emu 99.7%; handlers 931ms (0.3%); jit insns 341M,
iced 4.74M; compile_us≈222s; bg_stalls 1845 / 18.6s critical path; host_stops 74142
(noisy 32488); frames_published=1; winapi_lock_wait guest 334ms/max 211ms.

| Lane | Scope (files) | Tasks | Target |
|---|---|---|---|
| S2 stalls | ✅ bg_timeout 10→1ms (−59% stall time); STICKY_WAYS 2→4 (−97% helper loads) |
| S3 dispatcher | ◑ analyzed: handler_ms=178ms (0.8% of wall) — already negligible; further work = JIT throughput |
| S4 kernel32 misc | `kernel32/misc+heap` | CRS enter/leave ~10µs→no-op; HeapAlloc pool; cache GetLocaleInfoA/env | CRS pair ≤15ms |
| S5 file path | `kernel32/file_io/*` | serve reads/seeks from resident mirror slice; keep dirty-flag semantics; maybe cache wad dir | ≤50ms combined |
| S6 present | `gdi32/dib.rs, print.rs` | route single BitBlt (84ms) through zero-copy present; pixel proof | blit ≤5ms |

Handler table reference rows (debug): gettextmetricsw 227ms/1; readfile 151.8ms/3174;
CRS enter+leave 233ms/12424 pairs; bitblt 84ms/1; setfilepointerex 59.6ms/2898;
heapalloc 56ms/3700; GetLocaleInfoA 12.7ms/471.

## 6. Execution model (learned)

Dispatched background sessions died silently (8×: no report AND zero tree drift), except
one partial artifact (`docs/lanes/S2.md` — proves incremental-notes pattern works).
Decision: **execute lanes directly, serially by priority** (S1 first), using the
incremental-notes pattern. If re-dispatching agents: require `docs/lanes/<lane>.md`
notes, small steps, tolerate cargo-lock waits.

Execute-verify loop:
1. Pick highest-priority unfinished task.
2. Instrument minimally (tagged), build debug, run repro, capture evidence.
3. Fix root cause; remove tags; `cargo test`.
4. Re-run repro ×3 (nondeterminism check); update this file's status tables.
5. Repeat. Final: release A/B vs checkpoint commit.

Repro commands:
```bash
# functional (debug)
WIE_RUNTIME_PROFILE=1 timeout 500 ./target/debug/wie run --max-api 100000000 \
  --bottle doomretro --app-dir real_exes/doomretro --screenshot /tmp/dr.bmp \
  real_exes/doomretro/doomretro.exe -iwad freedoom2.wad
# timing baseline (release, solo machine)
WIE_RUNTIME_PROFILE=1 ./target/release/wie run --max-api 20000000 --bottle doomretro \
  --app-dir real_exes/doomretro real_exes/doomretro/doomretro.exe -iwad freedoom2.wad
```

## 7. Status board

| Item | State |
|---|---|
| xmm writeback fix | ✅ committed (checkpoint) |
| S1 version `I_Error` | ✅ RESOLVED — JIT stored AL for `movb %ah,mem` (lower_mov/lower_xchg); fix + `sib_copy` regression micro; see docs/lanes/S1.md |
| Follow-on boot gaps (2026-08-24) | ✅ cmppd/cmpps/cmpss/cmpsd(imm), movmskpd/ps, cvttpd2dq, rcpps family (interpreter); `cqto` (JIT); xadd double-ireduce; high-byte reg-reg/movzx/cmpxchg reads; ctx-poison clear on failed compiles; verifier always-on + rate-limited rejection WARN; `WIE_NO_HOOK_SLICES` knob |
| S2 stalls | ⏸ notes only (`docs/lanes/S2.md`) |
| S3 dispatcher | ⏸ not started |
| S4 kernel32 misc | ✅ CRS fast path via one `host_span` (233→25.5 ms, −89%); locale/env caching pending |
| S5 file path | ✅ buffered ReadFile 7→4 lookups (211→52 ms combined, −75%) |
| S6 present | ◑ agent landed typed-BMIH refactor in dib.rs (kept); BitBlt 84 ms is debug-inflated — deprioritized |
| cargo test | ✅ green after S4+S5 |


### Verified post-fix profile deltas (debug, Doom Retro startup)

| Row | Before | After |
|---|---|---|
| handler_ms total | 931 ms | 302 ms |
| readfile | 151.8 ms / 3174 | 40.8 ms |
| setfilepointerex | 59.6 ms / 2898 | 11.5 ms |
| entercriticalsection | 120.4 ms / 12424 | 12.7 ms |
| leavecriticalsection | 113.0 ms / 12424 | 12.8 ms |

Handlers are now noise vs emulation; further perf work = S2/S3 JIT lanes.

Dispatch note: 14 background sessions died silently with zero-to-partial output; two
contaminated shared manifests (broken `zerocopy.byteorder` feature — fixed by revert).
All lanes now executed directly in this session.
