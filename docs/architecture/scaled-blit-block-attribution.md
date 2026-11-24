# Attribution of the scaled-blit block (`0x1800d8fb0`, 54.1 % of guest time)

SDL2's 16.16 fixed-point scaled blit is the single largest consumer in the
workload. Its per-iteration bill is **236 host instructions for 8 guest
instructions (29.5 : 1)**, and the block's 573-instruction function is only 41 %
of it: 62 instructions are paid once per entry and 244 once per exit.

| per iteration (236) | share | per entry / per exit |
| --- | --- | --- |
| memory-probe guard chains **94** (39.8 %) | ← the biggest item | entry **62** (prologue: 16 GPR loads, pin/gen hoists, hook predicate) |
| flag construction **88** (37.3 %) | | exit **275** (rip + 16-GPR writeback, chain lookup, retabs) |
| loop control + back-edge state shuffling **36** (15.3 %) | | cold probe-miss paths **31** in the same function |
| 16.16 stepping / guest ALU **11** (4.7 %) | | |
| address arithmetic **7** (3.0 %) | | |

Two things fall out of it, both answerable without another run:

1. **39 instructions per iteration (16.5 %) are a flag word that is provably
   dead** — written, then cleared by the next writer's mask before any read, and
   never stored. The fix is to move that pack into the cold miss path, not to
   delete it: the fault path needs architectural flags.
2. **The probe guard chain can be amortised across the loop, soundly, for
   −64 instructions per iteration (27.1 %)** — the invalidation contract is
   already strong enough: every mapping/protection change bumps `mem_gen`, so a
   per-iteration generation compare plus a per-operand page-key compare is a
   complete guard.

Both are bigger than everything else on the table. Details, proofs and the
per-fix verdicts below.

Date 2026-11-24, HEAD `bd9a5c7`, host macOS 26.5.2 / Apple Silicon, release
build, `opt_level=none`, `WIE_JIT_REGION` off. **No new runs were needed**: the
CLIF and the machine code for this block came from the previous slice's
100-second run (`/tmp/x2.clif`, `/tmp/x2asm.txt`), so the host stayed idle.

---

## 1. Method, and why it is two methods

**Method A — phase-exact machine-code attribution.** `WIE_JIT_MAP` gives the
block's host address; `lldb -p <pid> -o 'disassemble --start-address …'` gives
its 573 instructions. The function is segmented into 18 phases by the boundaries
visible in the code itself (each guest instruction's lowering, each probe, each
exit block). Every one of the 573 instructions lands in exactly one phase —
`0` uncovered — so the phases are a *partition*, not a set of overlapping
slices. The loop's back edge is `b 0x17bb480f8` at `0x17bb48868`; the code
between the two is the per-iteration path, the rest is entry, exit and cold.

**Method B — CLIF cones and block classes.** From the CLIF dump of the same RIP:
a backward slice from the store into `JitCtx+128` (the packed flag word) gives
the flag cone independently of any mnemonic rule; the 43 CLIF blocks classify by
inspection into entry / guest body / probe / exit.

The two agree where the mapping is 1:1 and differ where it is not, and that is
the useful part:

| category | machine code (of 573) | CLIF (of ~668 ops) | agreement |
| --- | --- | --- | --- |
| entry (prologue) | 62 — **10.8 %** | 82 — **12.3 %** | ±1.5 pp |
| probe guard chains | 94 — **16.4 %** | 123 — **18.4 %** | ±2 pp |
| guest body (flags + ALU + address) | 236 — **41.2 %** | 188 — **28.1 %** | **13 pp** |
| exit + cold | 275 — **48.0 %** | 288 — **43.1 %** | ±5 pp |

The body gap is the expected one and is not a measurement error: `opt_level=none`
means the *backend* is not the optimiser but it still folds
`iadd_imm(ctx, off) + load` into `ldr xd, [x0, #off]` (zero instructions for the
address build), folds `icmp`+`band` chains, and materialises one CLIF `iconst` as
1–4 `mov/movk`. The exit path is almost pure loads and stores, which map 1:1, so
its two counts agree. **Where the two methods can disagree, the machine code is
the number to quote.**

---

## 2. The per-iteration bill

```
guest                                     host (per iteration)   phase
mov   %rdx,%rcx                            4                    P4
add   %r10,%rdx                           40   (1 = result, 39 = flag word)  P2
shr   $0x10,%rcx                           5   (2 of them dead constants)    P3
mov   (%r9,%rcx,4),%ecx                   44 + 11                 P5 + P7
mov   %ecx,(%rax)                          3 + 39                 P8 + P9
add   $0x4,%rax                            6   (result only: flags dead)    P11
sub   $0x1,%r8d                           48   (10 = 32-bit masks, 38 = flags) P12
jne   0x1800d8fb0                          2                    P13
[back edge: 12 spill stores, 11 reloads, 4 moves, inv-gen check, b]   36   P16 + P17
                                          ---
                                           236
```

**Which part scales with pixels:** everything in that table — 236 instructions per
*pixel step*. The 62-instruction prologue is paid once when the block is entered
(the guest loop is a self-loop, so the chain hops in once per run) and the
275-instruction exit once when it leaves. For a 320-pixel row the fixed part is
306 of 75 826 instructions — **0.4 %** — so no measurement of this block should
ever quote the 573-instruction function as its cost.

Per-iteration composition, by mnemonic class (overlapping with the phases above,
which is how the flag share is checked):

| class | count | share |
| --- | --- | --- |
| other ALU (probe hash/index, guest arithmetic, 32-bit masks) | 58 | 24.6 % |
| `[sp]` spill/reload | 45 | **19.1 %** |
| branch / call | 25 | 10.6 % |
| `cmp`/`tst` + `cset`/`csel` + `adds`/`subs` | 40 | 16.9 % |
| flag-mask bit ops + `orr lsl` pack + `uxtb` | 43 | 18.2 % |
| constant build (`mov`/`movk`) | 13 | 5.5 % |
| memory access (non-sp) | 12 | 5.1 % |

**`[sp]` is 19.1 % of the iteration** — and 23 of those 45 are the back edge's own
state shuffle (§4, item 5), i.e. the block path re-spilling its 17-value state on
every trip around a loop the region lane would keep in SSA.

---

## 3. Category attribution, phase by phase

| phase | range | host insns | category |
| --- | --- | --- | --- |
| P1 prologue | `…80000–…800f4` | 62 | **per entry** |
| P2 `add rdx` result + flag word #1 | `…800f8–…80194` | 40 | flags (39) + guest ALU (1) |
| P3 `shr rcx` result | `…80198–…801a8` | 5 | 16.16 stepping (2 dead constants) |
| P4 `mov` + load address (`madd`) | `…801ac–…801b8` | 4 | 1 spill + 1 stepping + 2 address |
| P5 source-load guard chain | `…801bc–…80268` | 44 | **probe: 5 glue + 16 stack-pin + 23 refill** |
| P6 load miss path | `…8026c–…80298` | 12 | cold |
| P7 load hit arms | `…8029c–…802c4` | 11 | probe |
| P8 store address | `…802c8–…802d0` | 3 | address |
| P9 store guard chain | `…802d4–…8036c` | 39 | **probe: 16 stack-pin + 23 refill** |
| P10 store miss path | `…80370–…803b8` | 19 | cold |
| P11 store + `add rax,4` | `…803bc–…803d0` | 6 | store (3) + stepping (1) + spill (2) |
| P12 `sub r8d` + masks + flag word #2 | `…803d4–…80490` | 48 | flags (38) + 32-bit masking (10) |
| P13 `jne` consume | `…80494–…80498` | 2 | loop control |
| P14 exit path | `…8049c–…80694` | 127 | **per exit** |
| P15 writeback/retab variants | `…80698–…807e0` | 83 | per exit |
| P16 loop-continue reloads + inv-gen | `…807e4–…80824` | 17 | loop control (11 reloads, 5 inv-gen) |
| P17 back edge | `…80828–…80868` | 17 | loop control (12 stores, 4 moves, `b`) |
| P18 final exit + epilogue | `…8086c–…808f0` | 34 | per exit |

**Cross-check of the flag share, two methods, per iteration:** the phase
partition gives **88 / 236 = 37.3 %**; the mnemonic rule that produced the
whole-block 21.6 % in the previous slice, applied to the hot path only, gives
**75 / 236 = 31.8 %**; the difference is the 10 `orr lsl` inserts and 3 mask
constants at the end of each pack, which the narrow rule excludes.

**And this is why the 21 % and 38 % numbers were both right.** The whole-function
share is 88/573 = 15.4 % by phase and 21.6 % by mnemonic class; the *per-iteration*
share is 31.8–37.3 %. A block that is 41 % prologue-and-exits dilutes its own flag
share. Quote **37 % for throughput** and **21 % for "how much of the emitted code
is flags"** — the audit's original 38 % was the throughput number, and my earlier
correction to 21 % was the emitted-code number. This document supersedes that
correction for the throughput question.

---

## 4. The two hypotheses, settled

### 4.1 Can the probe guard be amortised across a loop? **Yes — and it is the biggest item.**

What the guard is, from the code (`…801bc–…80268`, and identical for the store):

```text
stack-pin stage (16)                      refill probe (23)
  ldur  x0,[sp,#0x78]   ; pin.gen          and   x0,x1,#0xfff    ; page_off
  and   x3,x0,#1        ; pin live?        lsr   x3,x1,#12       ; page_key
  ldur  x0,[sp,#0x90]   ; pin.lo           mov/movk ×4           ; GOLDEN  ← 4 insns
  sub   x4,x1,x0                           mul   x4,x3,x4
  tst   w2,#0xff ; b.ne → range checks     lsr   x4,x4,#51
  … 4 predicates, one branch each …        and   x6,x4,#0x1fe0   ; slot offset
                                           ldr x5,[x26,x6]       ; slot.tag
  glue (5): the two stages share the       ldr x4,[x26+8+x6]     ; slot.base
  same address and the same predicate      ldr x7,[x26+16+x6]    ; slot.prot
  conjunction                              ldur x14,[sp,#0x68]   ; mem_gen
                                           eor  x6,x3,x14 ; cmp x5,x6
                                           and  x3,x7,#(R|W) ; cmp
                                           add  host,x4,x0
```

**Soundness of a hoist.** Three facts, all in the tree:

1. `refill_slot`'s tag is `page_key ^ mem_gen`, so **one compare covers page
   identity and mapping generation** (`mem.rs`, `RefillSlot`).
2. Every path that can change a mapping or a protection — `virtual_alloc`,
   `virtual_free`, `virtual_protect`, `mem_map_image` — stores the new guest
   generation into `shared.mem_gen` **and** calls `invalidate_tlb()`
   (`jit/cpu_engine.rs:87, 99, 118, 159`). So `mem_gen` is a *complete* signal
   for "any slot, pin or protection you hoisted is stale".
3. `hoist_refill` already loads `mem_gen` once per block entry and every probe in
   the block compares against that constant — the code is *already* built on
   "`mem_gen` is fixed for this unit"; today the unit is one block, and the
   back edge re-enters the block so it is re-read only once per entry.

So the hoist is: at loop entry, run the full probe once per operand and keep
`(page_key, host_base, prot_ok)`; then per iteration per operand check the page
key (3 instructions) and keep the stack-pin range check (3), and per iteration
check `mem_gen` once (3, shared by both operands). A failed check falls into
today's full probe unchanged — the same `guard_exit` failure edge the code
already has — so **no invalidation contract is modified, only the frequency of
the check.** Fault semantics are untouched: the miss path still goes through
`wie_jit_load`/`wie_jit_store` and `set_fault`.

**The bound, measured:** today 44 (load) + 39 (store) = **83**; hoisted
2 × (3 + 2 + 3) + 3 = **19**; saving **64 instructions per iteration = 27.1 %**.
Cost: one full probe per operand at loop entry (~83, amortised over the row
length). Fallback frequency in this loop: the destination page changes once per
1024 pixels, the source page once per `4096 / (4 × scale)` pixels.

**Owner:** `crates/wie-cpu/src/jit/lower/mem.rs` (the probe's shape and the hoisted
variant) plus the loop prologue/back edge — `lower/emit.rs` for the self-loop path,
`lower/region_emit.rs` for regions. `region.rs`/`region_emit.rs` are the region
lane's files; `mem.rs` and `emit.rs` are shared.

**Verdict: do.** It is the largest single item measured anywhere in this campaign
(27.1 % of the hottest block, and the same 2-operand shape is in blocks #2, #4, #5
of the top ten), and the invalidation argument is already satisfied by code that
exists.

### 4.2 Does the 16.16 stepping pay 5 ops for an OF nothing reads? **No — this block's shape is different, and the `shr` shape is absent from the top ten.**

* This block's `shrq $0x10` (P3, 5 instructions) materialises **no flags at all**:
  `FlagState` elides them because the next writer (`sub r8d,1`) overwrites them,
  and the machine code confirms it — the shift's block is 2 dead constant ops
  (`mov x3,#0x3f`; `and x3,x3,#0x10` ≡ 16), `lsr`, and a 2-instruction
  zero-count select. `doom_startup`'s 5-op OF belongs to a block where the shift
  is the *last* writer before the branch, so its flags survive.
* What this block *does* pay, on each of its two materialised words: **AF 5 +
  PF 10 + OF 2 + CF 2 = 19** instructions for flags that only ZF feeds. AF is
  `eor,eor,and #0x10,cmp,cset`; PF is the 10-instruction
  `and #0xff + 3×(lsr,eor) + and #1 + cmp + cset`.
* **Top-ten census** (all ten guest blocks decoded; flag writers, terminator, and
  any AF/PF consumer inside the block):

| guest block | share | flag writers | terminator | AF/PF consumer in block |
| --- | --- | --- | --- | --- |
| `0x1800d8fb0` | 54.1 % | 4 (add, shr, sub) | `jne` | none |
| `0x1800a1c60` | 27.4 % | 3 (add, sub) | `jne` | none |
| `0x1800ab2e1` | 11.9 % | 14 (add, dec, inc, test) | `jle` | none |
| `0x1800cef0b` | 14.6 % | 5 (add, sub) | `jne` | none |
| `0x1800ab355` | 5.3 % | 4 (add, inc) | `jmp` | none |
| `0x1400f8dd0` | 3.6 % | 3 (add, dec, test) | `jg` | none |
| `0x1401550c0` | 0.7 % | 3 (add, cmp, sub) | `jae` | none |
| `0x1400f8de3` | 0.1 % | 3 (add, cmp) | `jne` | none |
| `0x1400f8d9a` | 0.1 % | 7 (add, cmp, sub, test) | `jle` | none |
| `0x1400f8dc2` | 0.1 % | 6 (add, dec, imul, test) | `jg` | none |

So: **0 of 10 have a shift whose flags survive** (only #1 has a shift at all, and
its flags are elided), and **10 of 10 pay 15 instructions per materialised word
for AF+PF that nothing in the block reads.** Dropping AF/PF outright is
**refuted**: both images contain consumers — `lahf` 13, `sahf` 6, `jp` 85,
`jnp` 31, `pushfq` 3, `popfq` 7 — so the bits must be *carried lazily*, not
omitted (§5, item 2).

---

## 5. Ranked fixes

| # | fix | bound (per iteration) | how the bound was established | verdict |
| --- | --- | --- | --- | --- |
| 1 | **Hoist both memory probes to the loop prologue**, re-check page key per operand + `mem_gen` per iteration | **64 / 236 = 27.1 %** | the two guard chains are 83 instructions measured; the hoisted form is 19 (3+2+3 per operand + 3 shared); soundness from the `mem_gen` contract (§4.1) | **do** |
| 2 | **Move the dead flag pack into the cold miss path** | **39 / 236 = 16.5 %** | proof below | **do** |
| 3 | **Per-block `iconst` cache** (GOLDEN ×2, masks) | 8–10 / 236 = 3.4–4.2 % | 4 `mov/movk` per probe × 2, plus 5 mask constants; all are loop-invariant constants | **do** |
| 4 | Fold two constant pairs in the emitter | 4 / 236 = 1.7 % | `mov #0x3f; and #0x10` ≡ 16 and `mov #0xffffffff; and #1` ≡ 1 are `band(iconst, iconst)` with constant results | **do** |
| 5 | Back-edge state shuffle (12 stores + 11 reloads + 4 moves) | 27 / 236 = 11.4 % | counted; it is regalloc spilling the 17-value block state at the loop edge | **defer** — the region form already removes it; a block-path fix means live-in narrowing in `lower/analysis.rs` |
| 6 | Drop AF/PF | — | **refuted**: `lahf` 13, `sahf` 6, `jp` 85 in the images | **drop** |
| 7 | Lazy AF/PF carried across block edges (region-style `FlagState` threaded on the edge) | 15 / 236 = 6.4 % (pack #2's AF+PF) | AF 5 + PF 10 measured; consumers exist, so they must be carried | **defer** to the region/lazy-flags lane (`lower/flags.rs` + the edge contract in `lower/emit.rs` + `JitCtx`) |

### The proof for #2

The flag word is one register throughout the hot path. In this block it is `x20`:

```text
0x17bb480ec  ldr   x20, [x0, #0x80]   ; entry: carrier from JitCtx
0x17bb48160  mov   x7, #-0x8d6        ; pack #1: clear mask (~(CF|PF|AF|ZF|SF|OF))
0x17bb48164  and   x7, x20, x7
0x17bb48168 … 0x17bb48190   uxtb/orr ×12   ; insert all six bits
0x17bb48194  orr   x20, x3, x4, lsl #4 ; x20 = pack #1's word
0x17bb48460  and   x7, x20, x7        ; ← THE ONLY READ, with the same clear mask
0x17bb48490  orr   x20, x1, x2, lsl #4 ; x20 = pack #2's word
0x17bb4851c  str   x20, [x28, #0x80]  ; the only stores: exit paths
```

The only consumer of pack #1's value is the instruction that **masks off exactly
the bits it wrote** (`0x8d5 & ~(-0x8d6) = 0`), and the value is not stored
anywhere in the hot path (`str x20,[x28,#0x80]` occurs only at `…8051c`,
`…80710`, `…808b4`, all on exits). So pack #1 — `0x17bb480fc–0x17bb48194`, **39
instructions** — is dead on the hot path, and the reason it exists is the *fault*
path: if the load probe misses and the helper faults, the architectural flags of
the `add` must be in `JitCtx`. That is why the fix is "**emit the pack in the
miss block instead of before the guard**", not "delete it": the guard's `fail`
edge dominates, so the same CLIF predicates are available there.

Removable: **39 of 236 = 16.5 %**, with no change in fault behaviour.
Owner: the flush point — `lower/insn.rs` (`PendingFlags`/`flush_pending`) and
`lower/mem.rs` (`call_load`/`call_store`'s `fail` block).

### Why #1 and #2 compose

They touch disjoint phases: #1 removes instructions from P5/P7/P9 (94),
#2 from P2 (39). Together: 103 of 236 = **43.6 % of the per-iteration bill**, and
the two are independent — either can land alone.

---

## 6. Not measured, and what would change the numbers

* **The other nine blocks' per-iteration bills.** This document attributes one
  block. The same method needs one `lldb` attach per block on a live map; the
  phase boundaries differ per block, so it is per-block work, not a script.
* **The probe hoist's real saving.** 27.1 % is a *bound* from instruction counts,
  not a paired timing: it assumes the hoisted form is exactly 19 instructions and
  that page changes are rare (they are, by the arithmetic, but not measured).
  The measurement that settles it is a paired `WIE_*` A/B once implemented, or —
  cheaper — an instruction-count check on the emitted code of a prototype.
* **Whether #1 and #2 survive as percentages** once the other nine blocks are
  attributed: both scale with memory operands per iteration, so they should be
  larger in blocks #2/#4/#5 (1-2 operands, 6-10 guest instructions) and
  smaller in the memset block (8 stores, no flag chain).
* **The exit path (275 instructions, 42.6 % of the function)** is not
  attributable per iteration and was only bucketed, not dissected. It carries the
  16-GPR writeback, the chain lookup and the edge IC; a per-exit measurement is
  the right unit if exit cost ever matters.

## Appendix: reproducing

```sh
# the machine code of one block (run 2 of the previous slice; no new run needed)
WIE_JIT_MAP=/tmp/x2map.txt timeout -k 5 -s INT 120 ./target/release/wie run \
  --bottle doomretro --gui doomretro.exe -- -iwad 'C:\Program Files\DoomRetro\freedoom2.wad'
grep -E ' 0x1800d8fb0 ' /tmp/x2map.txt          # host address
lldb -p <pid> --batch -o 'disassemble --start-address <host> --count 700' -o detach -o quit

# the CLIF of the same block
WIE_JIT_DUMP_IR=/tmp/x2.clif WIE_JIT_DUMP_IR_RIP=0x1800d8f00-0x1800d9000 <same run>

# the top-ten guest blocks' flag writers and AF/PF consumers
llvm-objdump --triple=x86_64-pc-windows-msvc -d real_exes/doomretro/SDL2.dll \
  | grep -E '\b(lahf|sahf|daa|aaa|setp|setnp|jp|jnp|pushfq)\b'
```

Counting rules: a "phase" is a contiguous host-address range whose instructions
all serve one of the purposes in §3; the 18 phases partition the function with 0
instructions uncovered. `[sp]` and the mnemonic classes are *overlapping* views
and are labelled as such. The flag cone is a backward slice from the store into
`JitCtx+128`. "Dead" means: the only consumer of a value is an instruction whose
mask discards every bit it produced.
