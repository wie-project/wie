# Attribution of the scaled-blit block (`0x1800d8fb0`, 54.1 % of guest time)

SDL2's 16.16 fixed-point scaled blit is the single largest consumer in the
workload. Its per-iteration bill is **174 host instructions for 8 guest
instructions (21.8 : 1)**, and the block's 573-instruction function is only 30 %
of it: 62 instructions are paid once per entry and 337 once per exit or on a
probe miss.

| per iteration (174) | share | per entry / per exit |
| --- | --- | --- |
| flag construction **77** (44.3 %) | ← the biggest item | entry **62** (prologue: 16 GPR loads, pin/gen hoists, hook predicate) |
| memory-probe pin guard **31** (17.8 %) | | exit **275** (rip + 16-GPR writeback, chain lookup, retabs) |
| dead same-slot round trip **18** (10.3 %) | | cold: probe miss paths **31**, refill hashes **46** |
| guest work remainder (address, stepping, masks, value) **29** (16.7 %) | | |
| loop control **9** (5.2 %), edge moves **5**, live slots **5** | | |

Every share in this document is against the *executed* per-iteration path. The
number quoted in the previous revision, 236, was the layout count and included
the two inline refill hashes; §8.1 explains the correction and applies the same
rule to the second block.

Two things fall out of it, both answerable without another run:

1. **39 instructions per iteration (22.4 %) are a flag word that is provably
   dead** — written, then cleared by the next writer's mask before any read, and
   never stored. The fix is to move that pack into the cold miss path, not to
   delete it: the fault path needs architectural flags. The second block has the
   same disease in a different form: 23 of its 26 flag instructions are read only
   on the exit edge (§8.4).
2. **The probe guard chain can be amortised across the loop, soundly, for
   −20 instructions per iteration (11.5 %)** — the invalidation contract is
   already strong enough: every mapping/protection change bumps `mem_gen`, so a
   per-iteration generation compare plus a per-operand span compare is a complete
   guard. It is **15–16 instructions per memory operand per iteration** in all
   three blocks attributed here (§8.3, §9.4) — 31 for the two-operand blocks and 63
   for the four-operand one — so the saving scales with operand count and is worth
   **≈14.6 % of guest time** over 96.1 % of leaf samples.

Both are bigger than everything else on the table. Details, proofs and the
per-fix verdicts below.

This document attributes **three** blocks, not one: §1–§7 are `0x1800d8fb0`
(54.1 % of guest time, 174 host instructions per iteration, 8 guest instructions),
§8 is `0x1800a1c60` (27.4 %, 115 per iteration, 6 guest instructions) and §9 is
`0x1800cef0b` (14.6 %, 177 per iteration, 10 guest instructions) — together
**96.1 % of leaf samples**. The per-iteration rule, the counting rules and the
verdict vocabulary are the same in all three sections.

Date 2026-11-24, HEAD `bd9a5c7`, host macOS 26.5.2 / Apple Silicon, release
build, `opt_level=none`, `WIE_JIT_REGION` off.

Provenance of the numbers: block #1's CLIF and machine code came from the
previous slice's 100-second run (`/tmp/x2.clif`, `/tmp/x2asm.txt`), so the
host stayed idle for that part. Block #2 (§8) needed one new bounded run —
`timeout -k 5 -s INT 120`, one `sample` at t≈75 s, one `lldb -p` attach —
in a quiet window the user granted; files are `/tmp/x3map.txt`,
`/tmp/x3.clif`, `/tmp/x3asm.txt`, `/tmp/x3samp.txt`. No A/B timing was run, so
every percentage here is an instruction count, not a duration.

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

> **Correction (§8.1).** 174 of those 236 execute per iteration. The table above is
> a *layout* inventory: it includes the two inline refill hashes (46 instructions,
> reached once per 1024 iterations, not once per iteration), the non-hot hit arms
> (8) and the cold guard legs (8). On the executed denominator, block #1 is
> **flags 77 / 174 = 44.3 %**, **probe 31 / 174 = 17.8 %**, **back-edge shuffle
> 24 / 174 = 13.8 %** (18 of those dead). The mnemonic percentages in this section
> are left as first computed; §8.3 restates them.

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
partition gives **88 / 236 = 37.3 %** of the *laid-out* count; the mnemonic rule
that produced the whole-block 21.6 % in the previous slice, applied to the hot
path only, gives **75 / 236 = 31.8 %**; the difference is the 10 `orr lsl`
inserts and 3 mask constants at the end of each pack, which the narrow rule
excludes. On the executed denominator (§8.1) these read **77 / 174 = 44.3 %** and
**75 / 174 = 43.1 %**.

**And this is why the 21 % and 38 % numbers were both right.** The whole-function
share is 88/573 = 15.4 % by phase and 21.6 % by mnemonic class; the *per-iteration*
share is 31.8–37.3 %. A block that is 41 % prologue-and-exits dilutes its own flag
share. Quote **37 % for throughput** and **21 % for "how much of the emitted code
is flags"** — the audit's original 38 % was the throughput number, and my earlier
correction to 21 % was the emitted-code number. This document supersedes that
correction for the throughput question.

**And the throughput number is 44.3 %, not 37.3 %** (§8.1): the 37.3 % denominator
contained 62 instructions that do not execute per iteration. The audit's 38 % was
therefore right to within the width of the flag rule, and this document's own 37 %
was the last remaining error in this table.

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

**The bound, measured:** of the 83 instructions laid out for the two guard
chains, **31 execute per iteration** (15 for the load, 16 for the store — the
rest is the inline refill hash, which runs once per 1024 iterations; §8.1). The
hoisted form's *executed* cost is 2 × 4 + 3 = **11** (per operand: a span
compare and a branch, ×2; plus one shared `mem_gen` compare and branch), so
the saving is **20 instructions per iteration = 11.5 %** of block #1. The 19/21
figures in §7.4 are the *laid-out* comparison and remain the right numbers for
code size.
Cost: one full probe per operand at loop entry (~83, amortised over the row
length). Fallback frequency in this loop: the destination page changes once per
1024 pixels, the source page once per `4096 / (4 × scale)` pixels.

**Owner:** `crates/wie-cpu/src/jit/lower/mem.rs` (the probe's shape and the hoisted
variant) plus the loop prologue/back edge — `lower/emit.rs` for the self-loop path,
`lower/region_emit.rs` for regions. `region.rs`/`region_emit.rs` are the region
lane's files; `mem.rs` and `emit.rs` are shared.

**Verdict: do.** It is the largest *cross-block* item measured in this campaign:
the guard is 31 instructions per iteration in block #1, 31 in block #2 (§8.3 —
the same code, 15 per first operand and 16 per second), so it is a fixed
per-operand cost wherever it appears, and the invalidation argument is already
satisfied by code that exists. Ranked by share of a single block, flags are now
larger in block #1 (§8.7).

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
| 1 | **Hoist both memory probes to the loop prologue**, re-check the span per operand + `mem_gen` per iteration | **20 / 174 = 11.5 %** (§8.1: 31 executed guard instructions, hoisted form 2×4 + 3 = 11 executed. The earlier `62 / 236 = 26.3 %` counted the cold inline refills and is superseded; §7.4's 83->21 audit is the *code-size* number and still stands) | the two guard chains are 83 instructions measured; the hoisted form is 21 (4 pin + 5 refill per operand + 3 shared); soundness from the `mem_gen` contract (§4.1) | **do** |
| 2 | **Move the dead flag pack into the cold miss path** | **39 / 174 = 22.4 %** | proof below | **do** |
| 3 | **Per-block `iconst` cache** (GOLDEN ×2, masks) | 8–10 / 174 = 4.6–5.7 % | 4 `mov/movk` per probe × 2, plus 5 mask constants; all are loop-invariant constants | **do** |
| 4 | Fold two constant pairs in the emitter | 4 / 174 = 2.3 % | `mov #0x3f; and #0x10` ≡ 16 and `mov #0xffffffff; and #1` ≡ 1 are `band(iconst, iconst)` with constant results | **do** |
| 5 | Back-edge state shuffle (12 stores + 11 reloads + 4 moves) | **27 / 174 = 15.5 %, of which 18 are provably dead** (§8.6) | counted, then the pairs checked one by one: 9 same-slot load->store pairs with the register untouched in between | **defer** — the region form already removes it; a block-path fix means live-in narrowing in `lower/analysis.rs` |
| 6 | Drop AF/PF | — | **refuted**: `lahf` 13, `sahf` 6, `jp` 85 in the images | **drop** |
| 7 | Lazy AF/PF carried across block edges (region-style `FlagState` threaded on the edge) | 15 / 174 = 8.6 % (pack #2's AF+PF) | AF 5 + PF 10 measured; consumers exist, so they must be carried | **defer** to the region/lazy-flags lane (`lower/flags.rs` + the edge contract in `lower/emit.rs` + `JitCtx`) |

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

Removable: **39 of 174 = 22.4 %**, with no change in fault behaviour.
Owner: the flush point — `lower/insn.rs` (`PendingFlags`/`flush_pending`) and
`lower/mem.rs` (`call_load`/`call_store`'s `fail` block).

### Why #1 and #2 compose

They touch disjoint phases: #1 removes instructions from P5/P7/P9,
#2 from P2 (39). Together: **59 of 174 = 33.9 %** of the per-iteration bill
(20 from #1, 39 from #2), and the two are independent — either can land alone.
Row #2 is now the larger of the two in this block; #1 still leads the campaign
because it is the same 31 instructions in every hot block (§8.5).

---

## 6. Not measured, and what would change the numbers

* **The other seven blocks' per-iteration bills.** This document attributes the
  top three blocks (54.1 %, 27.4 %, 14.6 % of guest time); the rest of the top ten
  is still a census, not an attribution. The same method needs one `lldb` attach per block on a live map; the
  phase boundaries differ per block, so it is per-block work, not a script.
* **The probe hoist's real saving.** 11.5 % (and 17.4 % of block #2) is a
  *bound* from instruction counts, not a paired timing: it assumes the hoisted
  form is 11 executed instructions for two operands and that page changes are rare
  (they are, by the arithmetic — the stride and the page size give 1 in 1024 — but
  not measured).
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

## 7. Spec: hoisting the probe out of a loop

Not implemented — `lower/mem.rs` and `lower/emit.rs` belong to other lanes this
round. This section is written so that lane does not have to re-derive anything.

### 7.1 What already exists, and the assumption it already makes

* `HoistedPin` (`lower/emit.rs:889`) carries `{guest_base, guest_end, host_base,
  allow, live}` loaded **once per block**; `hoisted_pin_probe`
  (`lower/mem.rs:330`) already expresses its five conditions as guarded exits.
* `RefillProbe` (`lower/emit.rs`) carries `{table, mem_gen}`, also once per block;
  `hoist_refill` (`lower/mem.rs:376`) loads `ctx.mem_gen` there.
* Both are documented as valid "for the block", with the argument *"the
  generation only moves on a guest map/protect, which returns through the
  dispatcher"*. For a **self-loop block the block is the loop**, so the emitted
  code already relies on `mem_gen` (and therefore `pin_gen` and the refill slots)
  not moving while the loop iterates.

So the hoist introduces no new assumption. It makes that existing one **checked**
(3 instructions per iteration) instead of argued, and it moves the *resolution*
of the address out of the iteration while keeping the *validation* in it.

### 7.2 The abstraction: one interval per operand

Every stage the chain can win with already resolves to "guest `[lo, hi)` maps to
host `= guest + bias`":

| stage | `lo` | `hi` | `bias` |
| --- | --- | --- | --- |
| stack pin / data pin | `pin.guest_base` | `pin.guest_end − size + 1` | `pin.host_base − pin.guest_base` |
| refill slot | `page_base = addr & !0xfff` | `page_base + 4096 − size + 1` | `slot.base − page_base` |

The `hi` clamp is what keeps the existing cross-page guard: an access that would
run past the end of the page fails the interval test, exactly as
`page_off + size <= PAGE_SIZE` does today.

```rust
// lower/mem.rs
/// One stage's translation, published as a loop-carried home:
/// guest `[lo, hi)` maps to host `guest + bias`. Valid while `mem_gen` holds.
#[derive(Clone, Copy)]
pub(super) struct HomeSpan { pub lo: Value, pub hi: Value, pub bias: Value }

/// The hoisted fast path: `host = addr + bias` when `addr` is in the interval,
/// otherwise take `fail` (which holds today's chain, unchanged).
pub(super) fn home_span_probe(
    bcx: &mut FunctionBuilder<'_>,
    span: &HomeSpan,
    addr: Value,
    fail: Block,
) -> Value;

/// Publish the winning stage's span in the chain's hit/merge block.
pub(super) fn span_of_pin(bcx: &mut FunctionBuilder<'_>, pin: &HoistedPin, size: u32) -> HomeSpan;
pub(super) fn span_of_slot(bcx: &mut FunctionBuilder<'_>, addr: Value, slot_base: Value,
                           flags: MemFlagsData, size: u32) -> HomeSpan;

/// One `mem_gen` recheck per iteration, shared by every operand of the block.
pub(super) fn generation_recheck(bcx: &mut FunctionBuilder<'_>, ctx_ptr: Value,
                                 flags: MemFlagsData, gen_at_entry: Value) -> Value;
```

`refill_tlb_probe` and `hoisted_pin_probe` keep their signatures: the hoisted form
*replaces the call at the operand site* and leaves today's chain as the `fail`
continuation. Four call sites change shape in `call_load`/`call_store`
(`lower/mem.rs:527` onward), not their logic.

Where the span lives: the operand's `HomeSpan` is **loop-carried** — a block
parameter of the self-loop body / region header, exactly like the 17-value guest
state. Two of the three fields come free for a refill-resolved operand
(`hi = lo + 4096`, `size` is a per-operand constant), so the practical encoding is
`(page_key, bias)` = 2 parameters per operand, 4 for this block's 2 operands.

### 7.3 The emitted shape

```text
entry:                                     ; once per block entry — unchanged
  pins, refill.table, mem_gen, 16 GPRs     ; today's prologue
  home_A = span_of(chain for operand A)    ; one full chain per operand
  home_B = span_of(chain for operand B)
  jump body(home_A, home_B, …state…)

body(home_A, home_B, …):                   ; the loop
  g = load  ctx.mem_gen                    ; ┐ 3 instructions, once per iteration
  ok = icmp g, mem_gen_at_entry            ; │ for ALL operands
  brif ok, probing, exit_path              ; ┘  failure ⇒ dispatcher re-entry

probing:
  …guest body; each memory operand:
      host = home_span_probe(home_X, addr, fail => chain_X)
  …
  jump body(home_A, home_B, …)              ; back edge, spans carried

chain_X:                                    ; today's pin → refill → helper chain
  (host, home_X') = …                       ; 3 extra ALU in the *cold* block
  jump probing                              ; with home_X' published, one access later
```

Two properties that matter for review:

* **A miss is a branch, not a re-entry.** `refill_tlb_probe` already takes
  `fail: Block`; `guard_exit` emits `brif cond, cont, fail`
  (`lower/mem.rs:315`), and the caller's fail chain ends in the miss block that
  calls `wie_jit_load`/`wie_jit_store` and merges back through a block
  parameter. The hoisted form adds one edge of the same kind. No dispatcher round
  trip, no duplicated body, no second copy of the guest instructions.
* **One generation compare, not one per operand.** `mem_gen` is global; per-operand
  compares would be `3 × operands`. The failure edge goes to the block's existing
  **exit path** (state writeback, return to the dispatcher): the prologue re-runs
  and re-hoists on re-entry. That keeps the body single-copy and costs one round
  trip in the case that today cannot happen (no helper moves `mem_gen`).

### 7.4 Instruction budget, audited against this block's emitted code

| per iteration, per operand | today, executed | today, laid out | hoisted, executed |
| --- | --- | --- | --- |
| stack-pin stage (happy path: not pinned) | **15–16** (`…801c0–…8020c`) | 21 (`…801bc–…8020c`, incl. 4 cold legs) | 4 (2 compare+branch pairs) |
| refill probe | **0** — 1 in 1024 iterations | 23 (`…80210–…80268`) | 0 — 1 in 1024 |
| generation recheck (shared by 2 operands) | 0 | 0 | 3 |

Per operand, **the guard executes 15–16 instructions** (measured; identical in
both blocks, §8.3). Two operands plus the shared recheck: executed **31 → 11**,
saving **20 instructions per iteration = 11.5 %** of block #1's 174. Those are the
numbers an implementer should hold the change to.

The laid-out comparison is the *code-size* one and is the other half of the win:
**83 → 21**, −62 instructions of emitted text per block, because the inline refill
hash (23 per operand) and the non-hot hit arms and cold legs disappear from the
body. If the hit-arm glue (`…8029c–…802c4`, 11 laid out) collapses too, the code
gets −71; that saves nothing per iteration, because only 2 of those 11 ever run.
Per-operand loop-carried cost: +2 block parameters each, and the back edge already
materialises 12 stores + 4 moves for its 17 parameters, so +4 parameters is
~2 instructions — except in block #2, where the edge already pays 27 slot
accesses for its tuple (§8.6).

### 7.5 Where the generation check is emitted

* **Self-loop** (`lower/emit.rs`, the block `lower_self_loop_term` closes): at the
  top of the body, before the first address computation. Failure branches to the
  existing exit path.
* **Region** (`lower/region_emit.rs`): next to `emit_back_edge`'s invalidation
  guard, which is the same pattern (one load, one compare, one branch). That file
  is the region lane's; the check there should reuse `generation_recheck` rather
  than a second copy.
* The check compares against `RefillProbe::mem_gen` — the value already loaded in
  the prologue and used as `mem_gen_at_entry`. No new field on `JitCtx`.

### 7.6 The invalidation paths, as a test list

Must bump `mem_gen` **and** call `invalidate_tlb` — all four already do
(`jit/cpu_engine.rs`):

- [ ] `virtual_alloc` — `:87`
- [ ] `virtual_free` (release and decommit) — `:99`
- [ ] `virtual_protect` — `:118`
- [ ] `mem_map_image` — `:159`

Deliberately *not* generation-changing, and each needs a test proving the hoist
does not mis-read them:

- [ ] host-side `mem_write` / `host_slice_mut` — bytes change, mapping does not
- [ ] guest store to a page containing translated code — `invalidate_code_range`
      bumps the **code** generation, which the loop's own inv-gen guard still
      checks; the hoist must not weaken it
- [ ] `flush_instruction_cache` — same, code generation only

Tests to write (one per line, each is a guest program in the JIT test harness):

1. `hoisted_span_survives_a_page_resident_loop` — N iterations over one page take
   the span path (assert no slot loads inside the body, or a hit counter).
2. `a_page_crossing_republishes_the_span` — a loop that walks two pages reads
   correct bytes on both, and the fast path resumes after exactly one chain run.
3. `virtual_protect_mid_loop_is_seen` — a host-side protect to read-only mid-loop
   must fault the next guest write.
4. `virtual_free_mid_loop_faults` — the next access faults; no stale host write.
5. `virtual_alloc_reuse_gets_a_fresh_span` — free then alloc the same VA; the span
   must not resurrect the old host page.
6. `pin_refresh_inside_a_loop` — stack accesses that move the pins mid-loop stay
   correct (`pins_gen == mem_gen` is the invariant the `live` field rests on).
7. `smc_inside_a_loop_exits` — a self-loop that stores to its own bytes exits and
   re-decodes instead of running stale code.
8. `cross_page_access_takes_the_slow_path` — a 4-byte access at `page_end − 2`
   fails the interval test (`hi` clamp) and reaches the helper.
9. `slot_collision_between_two_operands` — two operands whose page keys map to the
   same refill slot both stay correct (the home holds the *base*, not the slot, so
   eviction is not a correctness event).

---

## 8. The second block, `0x1800a1c60` — attributed, and a correction that applies to §2 and §5

`0x1800a1c60` is SDL2's `SDL_memcpy4` inner loop: 6 guest instructions, 19 bytes,
**27.4 % of leaf samples**. The guest shape, from the shipped PE:

```text
movl (%rdx),%eax      ; 1 load, walks +4/iteration
addq $0x4,%rdx        ; flag writer
movl %eax,(%r9)       ; 1 store, walks +4/iteration
addq $0x4,%r9         ; flag writer
subq $0x1,%r8         ; flag writer
jne  0x1800a1c60      ; ZF only
```

Method is §2–§3's, unchanged: a 22-phase partition of the emitted function
(`0x15697c000–0x15697c884`, **546 instructions** — the audit's 550 is the same
function on an older build), 0 instructions uncovered, cross-checked against the
CLIF dumped for the same RIP window. One bounded run produced both; commands are
in the Appendix.

### 8.1 The denominator, and why it moved

Both blocks are guarded the same way, and the guard decides what "per iteration"
means:

```text
0x17bb481d4  tst   w2,#0xff ; b.ne …        pin live?      executed every iteration
0x17bb4820c  cbnz  x3, 0x17bb482bc         pin in range?  executed every iteration
0x17bb48210 … 0x17bb48268                  hash + 3 slot loads + gen eor + 2 br  (23)
0x17bb482bc  ldur x1,[sp,#0x80] ; mov x28,x8 ; ldr w0,[x1,x4]   <- the actual access
```

The 23-instruction refill is reached only when the pin fails. With a 4 B stride
and a 4 KiB page that is **once per 1024 iterations**, so it is *cold*, exactly
like the miss-path helpers. §2's table counted it as per-iteration — twice, once
for the load's chain and once for the store's — so block #1's total was 46
instructions too high, and P7's 8 non-hot hit arms and the 8 cold guard legs were
in there too:

| block #1 `0x1800d8fb0` | instructions |
| --- | --- |
| laid out between back-edge target and back edge | 236 |
| − two inline refill hashes (run 1/1024 iterations) | −46 |
| − non-hot hit arms (P7) | −8 |
| − cold guard legs inside P5 (pin-dead / wrap arms) | −8 |
| **executed per iteration** | **174** |

Block #2 stated the same way: the laid-out range `0x15697c0fc–0x15697c7c8` is
**436** instructions, of which **115 (26.4 %)** execute per iteration.

**This supersedes §2's 236 and every percentage in §5.** Restated on the executed
path, block #1: flags **77 / 174 = 44.3 %**, probe **31 / 174 = 17.8 %**, back-edge
slot shuffle **24 / 174 = 13.8 %** (18 of those 24 dead), loop control 9, edge
moves 5, guest work 29. So the three numbers this
campaign has been quoting are: **21.6 % of emitted code is flags** (unchanged),
**44 % of block #1's iteration is flags**, and the audit's **38 %** was a
throughput number measured over a range that also contained exit blocks — which is
why it landed between the two.

The same range bug is what inflated the audit's host/guest ratios. Its "loop body"
is everything between the back-edge target and the back edge, and that is where
the **exit path's flag pack, 16-GPR writeback and chain lookup are laid out**
(block #1: 210 of that range's 477 instructions are exit code). So the audit's
63.8 and 75.8 host instructions per guest instruction are ~3x the truth; the
executed ratios are **174 / 8 = 21.8** and **115 / 6 = 19.2**.

### 8.2 Per-iteration bill for one guest iteration

Per iteration is the number that matters for both blocks — they are re-executed
per pixel (block #1) and per copied word (block #2) — and everything in the table
below scales with that count:

| phase | range | insns | category |
| --- | --- | --- | --- |
| P0 prologue: `JitCtx` -> `[sp]` mirrors, pin descriptors | `…c000–…c0f8` | 63 | **per entry** |
| H1 source address + guard #1 (hot part 18 of 23) | `…c0fc–…c154` | 18 | address 4, probe 14 |
| L1 pin hit arm: base load, ctx move, the `ldr w0` | `…c1fc–…c204` | 3 | probe 1, move 1, value 1 |
| M1 value -> `w26` + source pointer `+4` | `…c208–…c214` | 4 | value 1, address 3 |
| H2 dest address + guard #2 | `…c218–…c260` | 19 | address 4, probe 15 |
| S dst pin hit arm + the `str w26` | `…c310–…c314` | 2 | probe 1, value 1 |
| A dest pointer `+4` | `…c318–…c320` | 3 | address 3 |
| F flag cone for `sub r8,1` | `…c324–…c388` | 26 | flags 26 |
| T ZF test + `jne` | `…c38c–…c390` | 2 | control 2 |
| B back edge: 14 reload + 13 respill + trip + inv-gen + moves | `…c734–…c7c8` | 38 | dead 24, live shuffle 14 |
| C1 cold: hash/TLB recheck, source | `…c158–…c1f8` | 41 | **1 per page crossing** |
| C2 cold: hash/TLB recheck + `wie_jit_store` miss arm | `…c264–…c30c` | 43 | **1 per page crossing** |
| X1 exit: EFLAGS pack | `…c394–…c3cc` | 15 | **per exit** |
| X2 exit: 16-GPR writeback + `JitCtx+0x88` | `…c3d0–…c45c` | 36 | per exit |
| X3–X7 + E exit: inv-gen, trip, chain match x5, lookup call, epilogue | `…c45c–…c884` | 212 | per exit (one arm) |

The 22-phase partition behind this table covers all 546 instructions with 0
uncovered; the table condenses it to the rows that matter, which is why the last
row is one *arm* of the exit path and the column does not add to 546.

```
                        per iteration
guest                   host                         phase
movl (%rdx),%eax        15 + 1                       H1 + L1     (probe 15, load 1)
addq $0x4,%rdx          3                            M1
movl %eax,(%r9)         16 + 1                       H2 + S      (probe 16, store 1)
addq $0x4,%r9           3                            A
subq $0x1,%r8           26                           F           (flags)
jne                     2                            T
[back edge]             38                           B
                       ---
                       115   for 6 guest instructions
```

**Fixed versus scalable.** Per invocation: entry 63, exit **~92** when the chain
match hits on the first slot (15 + 36 + 9 + 12 + 8 + 12) and **~176** when it
falls through to `wie_jit_chain_lookup`. So a 320-word copy costs
`63 + 92 + 320 x 115 = 36 955`, of which the fixed part is **0.4 %**. Same
conclusion as §2: never quote the function size as the block's cost.

Sanity cross-check on the method: the two blocks' per-iteration costs are
174 : 115 = 1.51 and their sample shares are 54.1 : 27.4 = 1.97. The residual
(~25 %) is unexplained and I am not going to explain it away; per *guest
instruction* the ratio is 1.13. Whatever the residual is, it is not the phase
partition, which sums to the function exactly for both blocks.

### 8.3 Category attribution of the 115

| category | block #1 `0x1800d8fb0` (174) | block #2 `0x1800a1c60` (115) |
| --- | --- | --- |
| **flag construction** (cone + pack) | **77 (44.3 %)** | **26 (22.6 %)** |
| **memory-probe pin guard** (hot part only) | **31 (17.8 %)** | **31 (27.0 %)** |
| **dead same-slot round trip at the back edge** | **18 (10.3 %)** | **24 (20.9 %)** |
| guest work remainder (address, 16.16 stepping / masks, load-store value, spill) | 29 (16.7 %) | 17 (14.8 %) |
| loop control (ZF test, inv-gen, trip charge, `b`) | 9 (5.2 %) | 11 (9.6 %) |
| register moves at the edge | 5 (2.9 %) | 5 (4.3 %) |
| live frame-slot traffic at the edge | 5 (2.9 %) | 1 (0.9 %) |
| | **174** | **115** |

Which is a different shape from block #1: **probe-dominant, not flag-dominant.**
The reason is arithmetic, not lowering — block #2 has *one* flag-writing
instruction whose flags survive (`sub r8,1`; both `addq`s are flag-dead and get
no cone at all), against block #1's three, one of which is a 32-bit `sub` that
also needs 10 narrowing masks.

The probe row being 31 in both columns is the whole point: **it is the same code.**
By operand it is 15 for the first memory operand and 16 for the second, in both
blocks, because it is one emitter function — the numbers differ only in their
denominator. Per guest iteration, per memory operand, the guard costs **15–16 host
instructions** wherever it appears.

### 8.4 The flag cone: 26 instructions, 23 of them exit-only

Per iteration the block pays a full six-flag cone:

```text
0x15697c328  sub  x27, x28, #1                  ; the guest result            (live)
0x15697c32c  subs x0,  x28, x1  ; cset x0,vs    ; OF                          (dead)
0x15697c334  subs x1,  x28, x1  ; cset x7,lo    ; CF                          (dead)
0x15697c33c  eor/eor/and #0x10/cmp/cset x1,ne   ; AF  (5)                     (dead)
0x15697c350  cmp  x27, #0       ; cset x11,eq   ; ZF                          (live)
0x15697c358  and #1<<63/cmp/cset x10,ne         ; SF  (3)                     (dead)
0x15697c364  and #0xff, 3x(lsr,eor), and, cmp, cset x28,eq ; PF (10)          (dead)
```

The in-block consumer is `tst w11,#0xff ; b.eq` — **ZF only**. The other five are
read only by the exit path's pack at `…c394`, which packs all six bits into the
word stored at `JitCtx+0x88` at `…c3dc`. On the loop-continue edge
(`…c734–…c7c8`) there is **no use of `x0`, `x1`, `x7`, `x10` or `x28`** — `x28` is
overwritten by `mov x28,x27` at `…c7b8` — and no store to `JitCtx+0x88`.

Two methods agree. In the CLIF, `brif` tests the `icmp eq` (ZF) value alone and
the six flag values are consumed by the pack block only; in the machine code the
same five registers have no reader on the continue edge. So **23 of the 26 flag
instructions are dead on every iteration but the last**, and they can be sunk into
the exit edges (both of them: `…c394` and the invalidated exit at `…c7cc`), where
`x27` and `x28` are still live.

This is the `shrq $0x10` shape, generalised. There it was one OF nobody reads;
here it is five flags nobody reads until the block is left. Same mechanism — the
cone is emitted where the *instruction* is, not where the *flag* is read — and it
is not a DoomRetro artefact: in the 17 functions dumped for the
`0x1800a1c50–0x1800a1c90` window (5 distinct blocks plus recompiles) **every one
computes exactly one `ssub_overflow` + one `usub_overflow` + one parity chain**,
i.e. 5 of 5 blocks materialise the full six-bit cone around a single flag-writing
guest instruction. A census of the actual top ten is **not measured** in this
slice; the window census plus the emitter shape is what I have.

### 8.5 Can the guard be amortised? The ratio that decides it

§4.1 answered yes and §7 specs it. This block supplies the number that makes it
non-negotiable:

* guard validation, executed **every** iteration: 31
* the refill it protects, executed **1 in 1024** iterations: 23 per operand
  (46 for the two), or 41 + 43 if the miss arm is reached as well
* per 1024 iterations: **31 744 instructions of validation against 46 of lookup —
  a ratio near 700 : 1.**

The guard is not protecting a frequent event; it is 700 : 1 overhead on the steady
state. And it is the *same* 31 instructions in every hot block measured in this
campaign, because every hot block has 1–2 page-resident operands walking a
constant stride.

The hoisted form's *executed* cost is 2 × 4 + 3 = **11** (per operand a span
compare and a branch, twice; plus one shared `mem_gen` compare and branch —
§7.4's shape), so the saving is **20 instructions per iteration**: 11.5 % of
block #1 and 17.4 % of block #2. Extrapolated over the two blocks that carry
81.5 % of leaf samples, that is **~11 % of total guest time** — §9.4 raises the
same weighted figure to **≈14.6 %** once a four-operand block is in the sample. That is comparable
to what the flag work removes over the same two blocks (39 on block #1, 23 on
block #2), not double it as the 26.3 % figure implied; the reason to do the hoist
first is that it is the same 20 instructions wherever it lands, while the flag
wins are per-block shapes.

### 8.6 The back-edge shuffle, and a proof that part of it is dead

The loop header takes the *whole* guest state as block arguments — 17 of them,
16 GPRs plus the flag word. In the CLIF: `block1(v2 … v18)`, reached by
`jump block1(…)` with 17 arguments immediately after `store.i64 136` (the
`JitCtx+0x88` flag store) and the 16-GPR writeback. Crossing that edge therefore
materialises the tuple:

```text
0x15697c734  ldur x21,[sp,#0x68]        0x15697c788  stur x21,[sp,#0x68]
0x15697c740  ldur x19,[sp,#0x70]        0x15697c784  stur x19,[sp,#0x70]
0x15697c744  ldur x15,[sp,#0x60]        0x15697c78c  stur x15,[sp,#0x60]
0x15697c748  ldur x14,[sp,#0x58]        0x15697c790  stur x14,[sp,#0x58]
0x15697c74c  ldur x13,[sp,#0x50]        0x15697c794  stur x13,[sp,#0x50]
0x15697c750  ldur x12,[sp,#0x48]        0x15697c798  stur x12,[sp,#0x48]
0x15697c754  ldur x9, [sp,#0x40]        0x15697c79c  stur x9, [sp,#0x40]
0x15697c758  ldur x8, [sp,#0x38]        0x15697c7a0  stur x8, [sp,#0x38]
0x15697c75c  ldur x6, [sp,#0x30]        0x15697c7a4  stur x6, [sp,#0x30]
0x15697c760  ldur x5, [sp,#0x28]        0x15697c7a8  stur x5, [sp,#0x28]
0x15697c764  ldur x4, [sp,#0x20]        0x15697c7ac  stur x4, [sp,#0x20]
0x15697c768  ldur x3, [sp,#0x18]        0x15697c7b0  stur x3, [sp,#0x18]
```

Twelve pairs, same slot on both sides, and no instruction between the load and the
store touches the register (only `x24`, `x2` and `x23` are written in between,
and `x24`'s own pair is the one that is **not** dead). Removing both halves of
each pair leaves every slot exactly as it was: **24 of the 38 instructions in the
tail, 20.9 % of the iteration, are provably dead.**

Block #1 has nine such pairs (18 of 174, 10.3 %), so the shape is universal but the
size is not. Cause: Cranelift materialising the 17-value block-param tuple at a
loop edge — the reload is its spill of values whose home is a frame slot. I have
not confirmed the mechanism inside the allocator (**not measured**); what is
measured is the pairing, which is all the deadness proof needs.

### 8.7 Ranked fixes

| # | fix | bound (block #2) | block #1 equivalent | how bounded | file | verdict |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | **Hoist the probe to the loop prologue** (§7's spec, with §7.4's executed budget) | **20 / 115 = 17.4 %** | 20 / 174 = 11.5 % | 31 guard instructions measured; hoisted form 2×4 + 3 = 11 executed | `lower/mem.rs`, `lower/emit.rs` | **do** |
| 2 | **Sink the five non-ZF flag materialisations into the exit edges** | **23 / 115 = 20.0 %** | n/a here (block #1 needs *deletion*, §5 #2) | no reader of `x0/x1/x7/x10/x28` on the continue edge; `x27`/`x28` live at both exits | `lower/flags.rs`, `lower/emit.rs` | **do** |
| 3 | **Drop the dead same-slot reload/respill pairs at the back edge** | **24 / 115 = 20.9 %** | 18 / 174 = 10.3 % | 12 exact pairs, register untouched in between | block-param edge contract (`lower/emit.rs` + `jit/ctx.rs`), or the region form | **defer** — the region report already measured 0 `JitCtx` GPR-slot accesses per iteration; settle it with the owed region A/B rather than a second change to an edge contract three lanes touch |
| 4 | Per-block `iconst` cache (GOLDEN hash, masks) | 0 per iteration (this block's hash is cold) | 10 / 174 = 5.7 % | 4 `mov/movk` per hash x2, plus masks | `lower/emit.rs` | do (already §5 #3) |
| 5 | Anything aimed at the 4 bound compares or the address build | — | — | they are the minimum for a 64-bit straddle-safe probe | — | **drop** |

Ranking note: on the executed denominator, block #1's *dead flag pack* (39 / 174 =
22.4 %, §5 #2) is now nearly twice its hoist (11.5 %). The hoist still leads the
campaign, because it is the same 31 instructions in every block while the pack is
a property of one block's guest code.

### 8.8 Verdict on the default-versus-knob question

**Make the hoist the default, not a knob.**

1. The cost it removes is identical in both blocks — 15–16 host instructions per
   memory operand per iteration — so it is a property of the emitter, not of a
   workload. A knob would be a workload judgement dressed up as a flag.
2. The applicability test is already the stride: `HomeSpan` (§7.2) is derived from
   a constant stride at emission time, and where the stride is unknown or at least
   a page the interval test fails and the code falls into today's per-access probe.
   So the "does not pay" case is a guard, not a configuration.
3. The win is the largest measured item that is *not one block's shape*: ~11 % of
   guest time over the two blocks holding 81.5 % of leaf samples, the same 20
   instructions per iteration wherever it lands, against ~22 % for block #1's
   dead flag pack, which is a property of that block's guest code.

---

## 9. The third block, `0x1800cef0b` — four memory operands, and the guard is per operand

`0x1800cef0b` is **14.6 % of guest time** and the last unattributed member of the
top three. It is a **10-guest-instruction fill loop with four dword stores per
iteration**, which makes it the first block in this campaign that is not a
two-operand loop — and that is what answers the question §8 left open.

### 9.1 Build provenance, and a caveat on comparing flag rows

All numbers in this section come from one snapshot,
`/tmp/wie-block3-a2c499f` = `target/release/wie` as built at 20:34 on
2026-11-24, i.e. the tree at **`a2c499f`** ("flag word built only at the cold
fault edge; fold self-loops at their latch") with `WIE_JIT_REGION` off. That
commit landed before this slice started and is inside the binary; `region.rs` was
edited again at 20:36, *after* the binary was linked, so the region lane's newest
work is **not** in these numbers. The binary was copied to `/tmp` before the run
because another lane was rebuilding `target/release/wie` at the time — the
snapshot is what was measured, and a later rebuild cannot change it.

Two consequences, both stated rather than papered over:

* **This is still the block-path baseline.** The latch folding in `a2c499f` and
  `162f73e` is region-gated; with the region off, the emitted block still has the
  shape §8 described — a prologue that mirrors `JitCtx` into `[sp]`, a 17-parameter
  self-loop header, an internal back edge, a 17-parameter writeback block. Block #3
  is therefore directly comparable to blocks #1 and #2 on probe and slot numbers.
* **It is *not* comparable on flag numbers.** `a2c499f` is `doom_startup`'s change,
  and it works: the six-bit pack is now emitted only on the cold fault and exit
  edges (§9.2 shows four of them, all cold). Blocks #1 and #2 were measured on
  pre-`a2c499f` binaries, where the pack was inline on the hot path. The flag row of
  §9.5 must therefore not be read as "block #3 has less flag work than the other
  two" — it has *different* flag work, on a newer emitter.

Run: one bounded invocation (`timeout -k 5 -s INT 120`), one `sample`, one
`lldb -p` attach at t≈40 s. Files `/tmp/x4map.txt`, `/tmp/x4.clif`,
`/tmp/x4asm.txt`, `/tmp/x4samp.txt`. The host was carrying a `cargo build` from
another lane (load average 6.7), which costs nothing here: every number below is an
instruction count, not a duration.

`WIE_JIT_MAP` records three compiles of this block (`0x134510000`,
`0x134574000`, `0x134588000`); the analysis reads the last, as in §8.

**One honesty note on the 14.6 %.** That share is the leaf-sample attribution this
slice was given; it is not re-derived here. The `sample` taken during the run
(9.5 KB) contains no frame in any of the block's three host ranges, so it
contributes nothing — the attribution rests on the disassembly and the CLIF below,
which is the same basis the previous two blocks were attributed on.

### 9.2 The function, phase by phase

`0x134588000–0x134588b88` = **739 instructions**: twelve hot phases plus the cold
and exit phases, 0 instructions uncovered. The back edge is
`0x134588ac8 b 0x134588104`, so the loop head is `…8104` — *inside* the entry
block, not at `…8000`.

| phase | range | insns | category |
| --- | --- | --- | --- |
| P0 prologue: `JitCtx` → `[sp]` mirrors, pin descriptors | `…8000–…8100` | 65 | **per entry** |
| D carrier decode: six `and/cmp/cset` from `[sp,#0x18]` | `…8104–…8150` | 20 | flags 18, slot 2 |
| O1 store #1: address + guard (18 hot + 4 cold legs) | `…8154–…81a8` | 22 | address 3, **probe 15**, cold 4 |
| O1 cold: hash/TLB recheck, helper call, fault cone + pack | `…81ac–…8290` | 58 | **per fault** |
| O1 hit arm | `…8294–…82a0` | 4 | probe 1, value 2, move 1 |
| O2 store #2: address + guard | `…82a4–…82f0` | 20 | address 5, **probe 15** |
| O2 cold: recheck + call + cone + pack | `…82f4–…8424` | 77 | **per fault** |
| O2 arms (pin, refill) | `…8428–…843c` | 6 | probe 1, value 4, cold 1 |
| O3 store #3: address + guard | `…8440–…848c` | 20 | address 5, **probe 15** |
| O3 cold: recheck + call + cone + pack | `…8490–…85c0` | 77 | **per fault** |
| O3 arms | `…85c4–…85d8` | 6 | probe 1, value 4, cold 1 |
| O4 store #4: address + guard | `…85dc–…8628` | 20 | address 5, **probe 15** |
| O4 cold: recheck + call + cone + pack | `…862c–…8760` | 78 | **per fault** |
| shared writeback block the four fault cones branch to | `…8764–…880c` | 43 | **per fault / exit** |
| O4 arms | `…8810–…8834` | 10 | value 4, live slot 2, probe 1, cold 2, `b` 1 |
| C counter + six-flag cone + `jne` | `…8838–…88a8` | 29 | flags 25, address 1, result 1, test 2 |
| X exit: EFLAGS pack, 16-GPR writeback, inv-gen, trip, chain match/lookup | `…88ac–…8a3c` | 101 | **per exit** |
| B back edge: 11 reload + 13 respill + trip + inv-gen + moves | `…8a40–…8ac8` | 35 | dead 22, live 5, inv-gen 5, trip 1, moves 3 |
| X2 invalidated exit | `…8acc–…8b88` | 48 | per exit |
| | | **739** | 0 uncovered |

**Executed per iteration:** 20 + 18 + 4 + 20 + 3 + 20 + 3 + 20 + 5 + 29 + 35 =
**177 host instructions for 10 guest instructions (17.7 : 1)**. Per entry 65; per
exit ≈ 101 on the first chain match (the 43-instruction block at `…8764` belongs to
the fault path, not to the exit). The fixed part is therefore ~166 instructions, so
the 739-instruction function stops being a cost estimate as soon as the loop runs
more than ~170 iterations — which it does.

The laid-out range between back-edge target and back edge is `…8104–…8ac8` =
626 instructions, of which 177 (28.3 %) execute — the same ~3.5× layout-to-execution
ratio that §8.1 found, for the same reason: the four cold refill probes, the four
fault cones and the fault writeback are all laid out inside it.

### 9.3 Two-method cross-check

The CLIF dump for the same RIP window contains 122 functions (the ten blocks of
this loop plus recompiles). The one that is block #3 is `u0:7815` (1101 lines):
**four `iconst.i64 4095`** (the page-size mask, once per memory operand), four
occurrences of `0x0001_800c_ef0b` (the block's own RIP, in the four fault packs),
and block heads `0`, `2`, `63`, `1` with `block2` carrying **17 parameters**.

Two things that only the CLIF shows:

* `block2` — the loop header — begins with the carrier decode:
  `v117 = band.i64 v98, 64`, `v119 = icmp ne v117, 0`, then 128, 1, 2048, 4, 16.
  So the decode is in the **loop header**, i.e. per iteration, which is what the
  machine code's back edge to `…8104` says.
* `v560 -> v564` … `v809 -> v564`: seventeen block-parameter aliases at the head of
  the loop, the same 16 GPRs + carrier tuple §8.6 measured for block #2.

### 9.4 The question: is the probe guard still exactly 31?

**No — it is 63, and the reason is the only thing that could make it differ: this
loop has four memory operands per iteration, not two.** Per operand it is
**unchanged**:

| block | memory operands / iteration | guard instructions, hot path | per operand |
| --- | --- | --- | --- |
| #1 `0x1800d8fb0` | 2 (one load, one store) | 31 | 15 + 16 |
| #2 `0x1800a1c60` | 2 (one load, one store) | 31 | 15 + 16 |
| #3 `0x1800cef0b` | **4 (four stores)** | **63** | 15 + 15 + 15 + 15 + 3 base loads |

The three base loads are the pin's host base, reloaded once per hit arm (`…8294`,
`…8434`, `…85d0`); the fourth operand reuses a register. Every guard has the same
five predicates, the same four `ldur` of the pin descriptor / lo / hi / live bool,
and the same branch-polarity split that §8 noted (one guard branches into its cold
legs, three branch out to theirs).

**So the invariant is per operand, and the campaign claim sharpens rather than
breaks: every memory-heavy loop pays 15–16 host instructions per memory operand
per iteration to validate a pin that fails once per 1024 iterations.** Hoisting
removes 15.75 → 4 per operand, so the saving scales with operand count:

| block | operands | probe today | hoisted (4/operand + 3 shared) | saved | share of iteration |
| --- | --- | --- | --- | --- | --- |
| #1 | 2 | 31 | 11 | 20 | 11.5 % of 174 |
| #2 | 2 | 31 | 11 | 20 | 17.4 % of 115 |
| #3 | 4 | 63 | 19 | **44** | **24.9 % of 177** |

That closes the default-versus-knob question on the evidence, not on the argument:
a knob would have to be set per *operand count*, which is a property of the guest
loop the emitter already knows, so the emitter should just do it. The one thing
that *would* justify a knob is a block whose stride crosses a page per iteration,
and `HomeSpan` (§7.2) already rejects that case at emission time.

Weighted over the three blocks, which hold **96.1 % of leaf samples**:
0.541 × 11.5 + 0.274 × 17.4 + 0.146 × 24.9 = **≈ 14.6 % of guest time**, up from
the ~11 % §8.5 estimated before a four-operand block was in the sample. The hoist
is not a DoomRetro-shaped win; it grows with the operand count, and the earlier
two-operand blocks were the *narrow* case.

### 9.5 Which block is register-lightest, and is the spill share inverted?

Frame-slot traffic on the executed path, all three blocks on the same rule (a slot
op is any `ldur`/`stur` against `[sp]` other than the prologue, entry loads and
exit writebacks):

| block | slot ops / iteration | of which dead | share | live guest values in the body |
| --- | --- | --- | --- | --- |
| #1 `0x1800d8fb0` | 23 | 18 | 13.2 % of 174 | 5 (rdx rcx r8 r9 rax) |
| #2 `0x1800a1c60` | 25 | 24 | 21.7 % of 115 | 4 (src, dst, count, value) |
| #3 `0x1800cef0b` | 28 | 22 | 15.8 % of 177 | **3** (ptr, count, value) |

**Block #3 is the register-lightest of the three, and it is *not* the worst
spiller** — the ordering is #1 (13.2 %) < #3 (15.8 %) < #2 (21.7 %), while the
live-value ordering is #3 < #2 < #1. So the inversion as stated does not
reproduce: the lightest loop sits in the middle of the three.

One correction to the premise: **8.4 % is not a number in this document.** The
frame-slot shares measured here are the 13.2 / 21.7 / 15.8 % above; §2's 19.1 % is
the mnemonic share of the *laid-out* 236 for block #1, not the executed path. The
14.8 % figure the question refers to is §8.3's "guest work remainder" row for block
#2, which is address and value work, not spill.

There is, however, a real shape here worth naming, and it is not about the live set
at all:

**The edge work does not scale with the live set — it materialises the full
17-value tuple every trip.** Block #3's loop carries three guest values, modifies
four of the seventeen block parameters, and still pays 11 reloads + 13 respills at
the latch: **22 of those 24 are provably dead** (eleven exact same-slot load→store
pairs, §9.6). That is the largest dead *fraction* of the three blocks, and it is
not the allocator doing worse with fewer values — it is the edge contract
re-passing the whole tuple, so a lighter loop pays the same absolute cost and shows
a bigger dead share. Naming it for the emitter's value live ranges: at
`opt_level=none` the latch does not narrow its block arguments to the values the
loop changed, and that is independent of register pressure.

### 9.6 Ranked fixes

| # | fix | bound (per iteration) | how bounded | file | verdict |
| --- | --- | --- | --- | --- | --- |
| 1 | **Hoist all four probes to the loop prologue** (§7's spec, unchanged) | **44 / 177 = 24.9 %** | 63 guard instructions measured; hoisted form 4 × 4 + 3 = 19 | `lower/mem.rs`, `lower/emit.rs` | **do** |
| 2 | **Stop decoding the carrier inside the loop** — it is loop-invariant and only cold edges read it | **20 / 177 = 11.3 %** | proof below | `lower/flags.rs` + `lower/emit.rs` (the flush point) | **do** |
| 3 | **Sink the five non-ZF flag materialisations into the exit edges** | **23 / 177 = 13.0 %** | the cone's only in-loop reader is `tst w22,#0xff` at `…88a4`; `x13/x15/x12/x23/x0` have no reader on `…8a40–…8ac8` | `lower/flags.rs` | **do**, and it composes with #2 |
| 4 | **Drop the dead same-slot reload/respill pairs at the latch** | **22 / 177 = 12.4 %** | eleven exact pairs, register untouched in between | block-param edge contract (`lower/emit.rs` + `jit/ctx.rs`), or the region form | **defer** behind the region A/B — unchanged from §8.7 |
| 5 | Load the store value once (`ldur x11,[sp,#0x48]` runs 4× per iteration) | 3 / 177 = 1.7 % | the slot is not written anywhere in the body | `lower/emit.rs` | defer — falls out of #4's narrowing, or of a cached value |
| 6 | Anything aimed at the four bound compares or the address build | — | — | — | **drop** |

**The proof for #2.** The carrier word lives in slot `0x18`. Inside the loop body
the only writes to `0x18` are the decode's own two spills (`…8110`, `…8148`), and
the back edge reloads it at `…8ac4` after reloading everything else — so the
decode's input is the same word every iteration and its six outputs are
loop-invariant. Its only consumers are the four cold fault packs, and the first of
those (`…8240–…8280`) is an **identity round trip**: it masks the same word with
`~0x8d5` and re-inserts the six bits it just decoded from it. In the CLIF the
decode sits in `block2` (the loop header) and its consumers in `block63` (a cold
block), which is the independent confirmation.

Removing it costs nothing on the fault paths — the packs already load the word —
and it removes 20 instructions from every iteration: 11.3 %.

### 9.7 What the third block changes in the campaign

* **The hoist's case is closed.** 15–16 per memory operand per iteration in three
  blocks with 2, 2 and 4 operands; 14.6 % of guest time over 96.1 % of leaf
  samples. Default, not knob.
* **The flag work is now three fixes, not one.** §5's dead-pack removal and §8's
  sink are joined by §9's loop-invariant decode. All three are the same disease —
  flag state materialised where it is *written* rather than where it is *read* — in
  three shapes. One line for `doom_startup`: *the cone is emitted at the writer, so
  every flag writer whose flags are not read in-block pays for them on every
  iteration; three blocks now show it.*
* **The latch tuple is the second-largest cross-block item** and the only one with
  a working alternative already measured: the region form reports 0 `JitCtx`
  GPR-slot accesses per iteration. That A/B is now the cheapest way to test a ~12 %
  item on all three blocks at once.

---

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

```sh
# slice 6: block #3 (0x1800cef0b), one bounded run; the binary is copied first so a
# concurrent rebuild by another lane cannot change what was measured
cp target/release/wie /tmp/wie-block3-a2c499f      # sha256 a9eadd04a3e100b6ca3697af...
WIE_JIT_MAP=/tmp/x4map.txt WIE_JIT_DUMP_IR=/tmp/x4.clif \
  WIE_JIT_DUMP_IR_RIP=0x1800cef00-0x1800cef40 \
  timeout -k 5 -s INT 120 /tmp/wie-block3-a2c499f run --bottle doomretro --gui doomretro.exe \
  -- -iwad 'C:\Program Files\DoomRetro\freedoom2.wad'
grep -E ' 0x1800cef0b ' /tmp/x4map.txt   # three compiles; read the last (0x134588000)
PID=$(pgrep -f 'wie-block3-a2c499f run' | tail -1)   # the wie pid, not the timeout wrapper
lldb -p $PID --batch -o 'disassemble --start-address 0x134588000 --count 900' -o detach -o quit
sample $PID 3 -file /tmp/x4samp.txt
# 739-instruction function; back edge at 0x134588ac8 -> 0x134588104

# slice 5: block #2 (0x1800a1c60), map + disassembly + CLIF in one bounded run
WIE_JIT_MAP=/tmp/x3map.txt WIE_JIT_DUMP_IR=/tmp/x3.clif \
  WIE_JIT_DUMP_IR_RIP=0x1800a1c50-0x1800a1c90 \
  timeout -k 5 -s INT 120 ./target/release/wie run --bottle doomretro --gui doomretro.exe \
  -- -iwad 'C:\Program Files\DoomRetro\freedoom2.wad'
grep -E ' 0x1800a1c60 ' /tmp/x3map.txt     # three addresses; the last compiled is the one to read
lldb -p <pid> --batch -o 'disassemble --start-address 0x15697c000 --count 600' -o detach -o quit
# 546 instructions, back edge at 0x15697c7c8 -> 0x15697c0fc
```

Counting rules: a "phase" is a contiguous host-address range whose instructions
all serve one of the purposes in §2 or §8.2; the phases partition the function
with 0 instructions uncovered. **Two counts are reported for every block: the
*laid-out* range (what the audit calls the loop body) and the *executed* path
(what runs per iteration).** They differ by the inline refill hash, the non-hot
hit arms and the cold guard legs — 62 instructions in block #1, and the
difference is what §8.1 corrects. `[sp]` and the mnemonic classes are
*overlapping* views and are labelled as such. The flag cone is a backward slice
from the pack that feeds the store into `JitCtx+128`. "Dead" means: no
consumer on the path the instruction is claimed dead on — for the exit-only flag
bits, the check is that the continue edge neither reads the register nor stores
to `JitCtx+0x88`; for the slot pairs, that the register is unmodified
between the load and the store of the same slot.
