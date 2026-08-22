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

## 4. Open blocker (Lane S1): version `I_Error`

Symptom: guest prints *"The wrong version of C:\Program Files\DoomRetro\doomretro.wad was
found."* then exits -1. Site: d_main.c:2863
`if (!M_StringCompare(s_VERSION,"DOOM Retro v6.3")) I_Error(...)`. `s_VERSION` set from
DEHACKED `[STRINGS]` (`src/d_deh.c`: deh_procStrings → deh_procStringSub → `*ppstr = value`).

Verified facts:
- `doomretro.wad` valid; DEHACKED lump @ file off 0x0c size 0x603b starting
  `"[STRINGS]\r\nVERSION = DOOM Retro v6.3\r\n..."`. Wad opened + read (214 OK ReadFile).
- Post-fix, crash gone; failure now nondeterministic across runs (v1 fail, v2 pass-until-
  API-budget-exhausted, v3 fail).

Static anchors already computed (exe sections):
```
.text  VA 0x140001000 raw? .rdata VA 0x140156000  .data VA 0x1401b2000 (ptr 0x1b0a00)
"VERSION\0" occurs exactly once in file @ 0x17fc10   (section mapping TBD — see note)
```
NOTE: a quick offset→VA script had SizeOfRawData/PointerToRawData swapped; redo mapping
with correct PE layout (+8 VirtualSize, +12 VA, +16 SizeOfRawData, +20 PointerToRawData).

Anchors (COMPUTED, correct mapping):
- `"VERSION\0"` @ VA `0x140180a10` (.rdata)
- `deh_strlookup` entry @ `0x1404bba50` (entry+8 = lookup ptr)
- **`s_VERSION` variable @ guest VA `0x1404bb560`** ([var] = char* of parsed string)
- `[VER]` probe installed at pump.rs ExitProcess handler (exit_code != 0 → dump ptr+content).

Method to find `s_VERSION`:
1. Map file 0x17fc10 → true VA (call it V_STR).
2. Scan `.data` raw for qword == V_STR at 8-byte alignment → that address is
   `&entry.lookup` (= entry+8); entry+0 holds `&s_VERSION` variable (call it S_VAR).
3. At runtime dump: `u64 p = mem[S_VAR]; string bytes at p`.
Hook point: pump.rs "guest exited with a non-zero code" site (or ExitProcess handler);
needs engine memory read access there.

Hypotheses: s_VERSION empty (VERSION key never matched), mis-parsed value, or compare
mis-execution. Watchpoint alternative: tag writes to `[S_VAR, +8)` in `GuestMemory::write`
(caveat: JIT fast-path stores bypass it).

## 5. Optimization lanes

Baseline (debug profile): wall 370s; emu 99.7%; handlers 931ms (0.3%); jit insns 341M,
iced 4.74M; compile_us≈222s; bg_stalls 1845 / 18.6s critical path; host_stops 74142
(noisy 32488); frames_published=1; winapi_lock_wait guest 334ms/max 211ms.

| Lane | Scope (files) | Tasks | Target |
|---|---|---|---|
| S2 stalls | `jit/config.rs`, `jit/lower/mod.rs` | per-RIP stall instrumentation ([STALL]); pre-warm hot successors; dedupe compiles(1819) vs bg(7225); audit never=144 | bg_stall_us < 2s |
| S3 dispatcher | `jit/pipeline.rs` | measure fixed cost incl. full-xmm-writeback; trim ctx build/stats; categorize host_stops | stops −30% |
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
| S1 version `I_Error` | 🔎 narrowing: compare receives non-text pointer slot (`0x207fe570`, first byte `'A'` = pointer low byte); CANDIDATE-deref probe running |
| S2 stalls | ⏸ notes only (`docs/lanes/S2.md`) |
| S3 dispatcher | ⏸ not started |
| S4 kernel32 misc | ✅ CRS fast path via one `host_span` (233→25.5 ms, −89%); locale/env caching pending |
| S5 file path | ✅ buffered ReadFile 7→4 lookups (211→52 ms combined, −75%) |
| S6 present | ◑ agent landed typed-BMIH refactor in dib.rs (kept); BitBlt 84 ms is debug-inflated — deprioritized |
| cargo test | ✅ green after S4+S5 |

Dispatch note: 14 background sessions died silently with zero-to-partial output; two
contaminated shared manifests (broken `zerocopy.byteorder` feature — fixed by revert).
All lanes now executed directly in this session.
