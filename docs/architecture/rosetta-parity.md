# Matching and beating Rosetta 2's translation quality

Rosetta 2 runs x86-64 macOS apps at roughly 70-80 % of native, which in these
units is **under one host cycle per guest instruction**. It is the existence
proof that the ceiling is real, and it is the bar the owner has set: match it,
then beat it where our position is better than Apple's.

This document is the inventory: what Rosetta has, what we have, what we are
missing, and where we can do better than it.

## 0. Measure the right thing

Correction to earlier work in this repo: **CLIF op counts are not a quality
metric.** `iadd_imm(ctx, off)` + `load` is folded into a displacement-form
access before encoding (`ldr x27,[x0,#0x80]`); a 997-instruction block contains
`zero` materialised frame addresses. Rewriting the lowering to emit explicit
displacements changes the CLIF text and *nothing* in the machine code.

The metrics that matter, both available today:

- `sample` on the guest thread + `WIE_JIT_MAP` → which guest blocks own the time.
- `lldb -p <pid> -o 'disassemble --start-address <blk> --count N'` on a live run →
  host instructions per guest block, and their *mix* (this is how the 38 %
  EFLAGS finding below was made).
- cycles per guest instruction = (block host instructions ÷ IPC) ÷ block guest
  instructions. Measured today on DOOM Retro's hot loop: **1433 host
  instructions per iteration for 36 guest instructions = 39.8 per insn**, at
  ~4 IPC and ~340 cycles/iteration → **~9.5 cycles per guest instruction**.

## 1. The inventory

Legend: **have** = shipped and measured; **part** = exists but weaker;
**miss** = absent; **better** = a place where our position beats Apple's.

### 1.1 Code shape

| Rosetta does | we do | action |
|---|---|---|
| Translates **regions/functions**, not basic blocks: one translation unit per function with a native CFG inside | one Cranelift function **per basic block**, chained by hops | **miss, biggest single item.** Two-block loops pay a hop + prologue/epilogue per iteration. Emit a *loop region* as one function with a native back edge |
| Keeps a **translation cache**, AOT-translates at first run then JITs | persistent code cache exists but is **off by default** and refused for chained blocks | **part.** Enable + measure once regions exist (regions are far more cacheable than blocks) |
| Translates on background threads while the app runs | 4 background workers, work-weighted promotion | **have** |
| **Superblock/trace formation** with side exits, hot-path layout, loop peeling | none | **miss** |
| Return-address prediction / shadow stack to make guest `ret` cheap | we *removed* an unread shadow stack (nothing consumed it) | **miss** — but only worth doing with region compilation, where a predicted `ret` can stay in native code |

### 1.2 Register allocation

| Rosetta does | we do | action |
|---|---|---|
| Guest GPRs live in **host registers across a translation unit**, real allocation | every block loads its live-ins from `JitCtx` and stores its dirty regs back; ~39.8 host insns/guest insn with 42 `ldp` + 45 `ldur` + 52 `str` + 17 `stur` of **spill traffic per 25-instruction block** | **miss.** Region compilation gives Cranelift a scope worth allocating over |
| Reuses host registers for guest `xmm` with alignment awareness | XMM as 32 scalar I64 halves, packed ops convert both ways | **part** — the split-pair round trip is a known cost (partly fixed; see `copy-and-lock-audit.md`) |

### 1.3 Flags

| Rosetta does | we do | action |
|---|---|---|
| Materialises **only the flags a consumer reads**, and often fuses compare+branch so no flag value exists | a packed `rflags` word, decoded per consumer: **~26 ops per `jcc`/`cmov`**, and ~38 % of a hot block's machine code is EFLAGS construction (`mov`/`cmp`/`csel`/`orr` per bit) | **miss, second biggest item.** In flight: `FlagState` for ordinary blocks, plus cmp+jcc fusion |
| Uses ARM condition codes / `csel` / `cset` directly for `cmov`/`setcc` | does use `csel`/`cset`, but only after decoding the packed word | **part** |
| Elides flags for `test` (a `test` + `je` needs no materialised ZF) | materialises per bit | **miss** |

### 1.4 Memory

| Rosetta does | we do | action |
|---|---|---|
| Direct-mapped guest→host translation with a **single guard per page**, TSO handled by hardware | 256-slot direct-mapped refill table in `JitCtx`, tag folds `mem_gen`, ~29 CLIF ops/operand (was 71), one helper call per miss (was ~100 %) | **part, recently fixed** — this was the single worst data structure and it is now the right one |
| **AOT-known code pages**, so stores to code are rare and cheap to trap | SMC page queue + invalidation generation, drain now gated on one atomic load | **have** |
| Huge pages / prefetch for code and data arenas | none | **miss, cheap** |
| **Hardware TSO**: x86 binaries run with `SCTLR_EL1.TSO` set, so no barriers are needed for ordinary x86 ordering | we are an ordinary ARM64 process; any ordering we need must be emitted or avoided | **miss, but not a perf item today** — our guest ordering comes from single-threaded hot paths; revisit only if profiling shows barriers. Rosetta's advantage here is a private kernel feature we cannot use. |

### 1.5 Instruction selection

| Rosetta does | we do | action |
|---|---|---|
| Emits `ldrb`/`ldrh`/`ldrsb`/`ldrsh` for zero/sign-extending loads; `ubfx` for shift+mask; `tbz`/`tbnz` for test+branch; `madd` for `imul`+`add`; `csel`/`cset`; `rev` for `bswap` | iced decodes to IR, Cranelift ISel picks forms; many of the above appear naturally | **part.** Audit the hot blocks' machine code against the guest's own optimal encoding — the 997-instruction block should be ~200 |
| **CSE + LICM inside a block** | none at `opt_level=none`: one frame slot loaded **14 times** in a 25-instruction block; 63 redundant accesses over 29 slots | **miss, cheap, high value.** Emitter-level value numbering over `JitCtx` loads within a block |
| Strength-reduces `lea` chains, folds address computation into loads | addressing folded (verified), `lea` left as arithmetic | **part** |
| Specialises on operand forms (reg/reg vs reg/mem) | one general lowering per mnemonic | **part** |

### 1.6 Environment leverage (where we can be *better* than Rosetta)

Rosetta must translate arbitrary code it did not write, for an OS it does not
own. We own the guest's whole Win32 world. That asymmetry is the honest path to
"better than Rosetta":

| opportunity | why it beats Rosetta | evidence / cost |
|---|---|---|
| **Native fast paths at API boundaries** — implement SDL2/GDI/D3D9 blit, fill, scale, present in host code instead of executing the guest DLL's pixel loops | Rosetta cannot: those are just instructions to it. `sample` puts **~47 % of DOOM Retro's JIT samples inside SDL2.dll's own blit/fill loops** | large but bounded; this is the single largest guest-side cost after EFLAGS |
| **AOT the loaded PE in the background** the moment it is mapped, with whole-function regions and real register allocation, while the guest runs interpreted | Rosetta AOTs, but its unit is a translation unit of *its* choosing and it must be conservative about code that might be patched; a PE we loaded has known sections, known relocations, and we already track page permissions | we already have `WIE_JIT_CODE_CACHE` + background workers; the missing piece is region formation |
| **Profile-guided specialisation of hot regions** using our own counters, at a threshold that pays | Rosetta 2 does not re-specialise aggressively | `tier_compiles=8` today; needs a work-weighted counter (see `compile-selection-policy.md`) |
| **Fold away the guest's own redundancy**: e.g. recognise a guest loop whose body is a palette scan and vectorise it | Rosetta cannot specialise per-guest semantics | **research only** — no name-based special cases; if it cannot be expressed generally it does not ship |
| **Direct-mapped guest VA == host VA** where macOS lets us (the PE sits at 0x140000000, well inside the 47-bit space) | Rosetta keeps a translation table; we measured ≤1.4 % on 7-Zip for the *old* per-access cost — that number is stale, since the probe it replaced just got 2.4× cheaper | re-measure after regions land, not before |

## 2. The order to do it in

Each row is a paired A/B against the previous row, on DOOM Retro frames and on
`7za` wall time. A row that does not pay does not ship.

1. **Flags out of the hot path** (~38 % of machine code in the hot block). In
   flight.
2. **Per-block value numbering / CSE** of `JitCtx` loads (63 redundant accesses
   per hot block; a slot loaded 14×). Emitter-only, no compile cost.
3. **Loop-region compilation**: one Cranelift function per hot loop with a
   native back edge, live-ins loaded once, prologue/epilogue once. Removes the
   hop *and* gives Cranelift a scope to allocate over. Expected to also cut
   compile cost (fewer functions).
4. **Chain-hop cost** for everything still block-to-block. In flight.
5. **Region-level register allocation**: guest GPRs in host callee-saved
   registers across the region; kills the spill traffic (42 `ldp` + 45 `ldur`
   per block).
6. **AOT region compilation at load time**, persisted in the code cache.
7. **Instruction-selection audit** against the guest's own optimal encoding,
   block by block, starting with the 997-instruction hot block.
8. **API-boundary native fast paths** (SDL2 blit/fill/scale) — the biggest
   guest-side item, independent of the JIT ladder.

Current position: **~9.5-11.5 host cycles per guest instruction**. Rows 1-5 are
what stand between that and 2-3; rows 6-8 are what make it stay there and what
can take specific workloads below 2.
