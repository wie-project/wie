# Census: the "62 per entry / 275 per exit" bill (337), attributed on 7ffc1f6

The brief carried a standing figure — **62 host instructions per entry, 275 per
exit, 337 in total, amortised at hops/entries = 7.47 for 45 host instructions per
iteration**, 21 % of a 219.1 per-iteration bill. This document attributes that 337
by cause from the emitted AArch64 of the three blocks that carry 96.1 % of leaf
samples. **The 62 reproduces to 1 %. The 275 does not. The 337 is a layout count,
not an executed bill.**

Verdict, because it decides the round: **no single cause is above 25 % of the
337** (84 instructions). The only group above that line is 171 instructions of
cold code that does not execute per exit, belongs to the probe lane, and runs
about once per 2 000 iterations. The largest *executed* single cause is 29
instructions. Per the brief's own rule, the round stops at the census: **no
source change was made for this slice and nothing was A/B'd.**

Provenance: release build of `7ffc1f6` (`cargo build -p wie-cli --release`),
binary copied to `/tmp/wie-census-27519697` before the run so a concurrent
rebuild by another lane could not change what was measured. One bounded capture
(`--screenshot-on-frame 20`, killed once the disassembly was in hand) with
`WIE_JIT_MAP`, `WIE_JIT_DUMP_IR`, `WIE_JIT_REGION=0`; three `lldb -p`
disassemblies of 1400 instructions into `/tmp/c_census_asm.txt`; CLIF into
`/tmp/c_census.clif`. Counters from a separate 60-second
`WIE_RUNTIME_PROFILE=1` run into `/tmp/c_prof.log`. Instruction counts are
load-insensitive. **No frame number is quoted and none was taken.**

Counting rules are the campaign's: unit = disassembled start → trailing `udf`
padding; per-iteration path = forward-reachable from the loop head ∩
backward-reachable from the self-loop back edge (`retab` terminates a path,
which matters — treating it as fallthrough inflates the path by 2× and is the
first mistake this census made). Every phase below was read at its addresses,
not inferred from a mnemonic histogram.

---

## 1. The three blocks as they exist on this tree

| | #1 `0x1800d8fb0` | #2 `0x1800a1c60` | #3 `0x1800cef0b` |
|---|---|---|---|
| guest sample share (standing) | 54.1 % | 27.4 % | 14.6 % |
| unit (fn start → `udf` padding) | 689 | 668 | 757 |
| **entry** `[fn start, loop head)` | **111** | **109** | **88** |
| per-iteration path | 237 | 211 | 333 |
| **exit spine** (terminator's exit edge → first `retab`) | **86** | **102** | **87** |
| conditional return arms (×2, trip flush + teardown + `retab`) | 22 | 23 | 21 |
| dispatcher-return block (inv-gen fail, depth cap) | 59 | 58 | 56 |
| cold probe-miss legs + untaken dual-super body copy | 174 | 165 | 172 |

The rows partition each unit with **0 instructions uncovered**
(111+237+86+22+59+174 = 689; 109+211+102+23+58+165 = 668;
88+333+87+21+56+172 = 757).

Two structural facts drive the rest:

* **The prologue is straight-line.** The first branch in each unit is the first
  *memory access's* guard, inside the loop. All of the entry executes, once per
  entry.
* **The exit is a single spine.** Terminator's not-taken edge → pack →
  rflags+GPR writeback → inv-gen guard → chain-depth check → hop accounting →
  `bl wie_jit_chain_lookup` → depth restore → trip flush → frame teardown →
  `retab`. The chained successor is resolved through the Rust helper on all
  three blocks; no inline edge-IC or chain-table probe survives in these
  spines (#2 has a 12-instruction inline table probe, #1 and #3 have none).

---

## 2. The census table

Weights are the leaf-sample shares normalised to 1 (0.563 / 0.285 / 0.152).
"Executed" says whether the instruction runs once per entry, once per exit, or
only on a miss/rare arm.

| # | cause | weighted | executed | % of 449 | % of 337 |
|---|---|---|---|---|---|
| 1 | **entry:** `pacibsp` + 6 `stp` + `mov x29` + `sub sp` | 7.0 | per entry | 1.6 % | 2.1 % |
| 2 | **entry:** `JitCtx → guest GPR` loads (`ldr xN,[ctx,#0x0..0x80]`) | 24.0 | per entry | 5.3 % | 7.1 % |
| 3 | **entry:** those same GPRs spilled to frame slots (`stur xN,[sp,#…]`) | 25.1 | per entry | 5.6 % | 7.4 % |
| 4 | **entry:** flag materialisation for the header params | 6.5 | per entry | 1.5 % | 1.9 % |
| 5 | **entry:** probe-hoist guard arithmetic (span/allow/gen, per operand) | 44.3 | per entry | 9.9 % | 13.1 % |
| 6 | **exit:** pack of the flag word + PF ladder | 24.0 | per exit | 5.3 % | 7.1 % |
| 7 | **exit:** RIP store (`mov`/`movk`/`movk`/`str [ctx,#0x88]`) | 4.0 | per exit | 0.9 % | 1.2 % |
| 8 | **exit:** rflags + GPR writeback, and the frame reloads feeding it | 29.3 | per exit | 6.5 % | 8.7 % |
| 9 | **exit:** inv-gen guard (2 `ldr` + `ldar` + `cmp` + `b.ne`) | 5.0 | per exit | 1.1 % | 1.5 % |
| 10 | **exit:** chain depth check + bump | 5.0 | per exit | 1.1 % | 1.5 % |
| 11 | **exit:** inline chain-table probe (block #2 only) | 3.4 | per exit | 0.8 % | 1.0 % |
| 12 | **exit:** hop accounting + helper call + depth restore | 9.0 | per exit | 2.0 % | 2.7 % |
| 13 | **exit:** trip flush | 3.0 | per exit | 0.7 % | 0.9 % |
| 14 | **exit:** frame teardown + `retab` | 8.0 | per exit | 1.8 % | 2.4 % |
| 15 | conditional return arms (×2) | 22.1 | only when armed | 4.9 % | 6.6 % |
| 16 | dispatcher-return block | 58.3 | only when armed | 13.0 % | 17.3 % |
| 17 | **cold** probe-miss legs + untaken dual-super body copy | 171.1 | per miss | 38.1 % | 50.8 % |
| | **total** | **449.2** | | | |

Rows 1–14 are the executed bill: **103.2 per entry** (rows 1–5) + **87.3 per
exit** (rows 6–14) = **190.5**, or **197.6** if the entry is taken as its full
measured size (103.2 + 87.3 vs 107.0 + 90.7 — the table's row split is exact per
block, so both columns are the same numbers seen through two roundings; the
authoritative figures are the per-block counts in §1).

The rows sum to **449.2**, not 337. That gap is the finding.

---

## 3. The 62 reconciles to 1 %; the 275 does not

**Entry, the 62.** The standing description is "prologue: 16 GPR loads, pin/gen
hoists, hook predicate" — two different things. Measured: rows 1+2+3+4 =
7.0+24.0+25.1+6.5 = **62.6**, against the standing **62**. That is the
prologue's *architectural-state* portion (frame setup, 16 `JitCtx →GPR` loads,
the frame spills that hold them, and the flag bits of the header params). The
other **44.3** of the prologue — row 5 — is the per-operand span/pin/allow guard
arithmetic, which the campaign accounts under *probe addressing*, not entry. So
the prologue really costs **111 / 109 / 88 = 107 weighted**, and "62 per entry"
is right about a part of it.

**Exit, the 275.** The entire exit side is rows 6–16 =
87.3 + 22.1 + 58.3 = **167.7** weighted. There is nothing else on an exit path:
after `retab` the block is gone. To reach 275 you must charge **≈ 107
instructions of cold probe-miss code** to every exit. That is what the older
document did — its own wording is "paid once per exit **or on a probe miss**"
(`scaled-blit-block-attribution.md`, intro), and its P14+P15+P18 = 244 came from
a revision where the chain machinery was inline in the epilogue and shared an
address range with the probe legs.

**The bill.** Executed per entry + per exit = 197.6 weighted (107.0 + 90.7).
At today's measured `hops / entries = 980 771 813 / 129 980 681 = 7.546`
(brief: 7.47, same ratio from a 100 s run), that is **26.2 host instructions per
iteration**, not **45**.

Independent check: `insn_per_entry = 223.6` (dynamic retired guest instructions
per block entry) and the per-iteration path is 234.6 weighted, so 26.2 is
**11 %** of the per-iteration bill, not 21 %.

---

## 4. Why 337 is a layout number

337 sits between the executed 197.6 and the layout class 449.2 because it was
computed as a *remainder*: the disassembled unit minus the per-iteration path
minus the prologue's state-load portion. On this tree that remainder is
**341 (#1) / 348 (#2) / 336 (#3)** — so the 337 reproduces to 1–2 % *as a
remainder*, which is why it looked solid for several rounds. But only
167 / 183 / 164 of it is on an exit path (spine + arms + dispatcher); **174 / 165
/ 172 is cold**:

* every unit contains **two full copies of the guest body** — the dual
  super/normal path. The block-wide stack guard picks one per entry; the other
  never runs for that guest state and sits in the remainder;
* the **probe-miss legs**: `bl wie_jit_load` / `wie_jit_store` and their spill
  and `retab` tails.

How often the cold group runs, from the counters: `load = 7 223 650` and
`store = 7 668 137` helper calls against `28 994 423 336` retired JIT
instructions — **0.05 % of retirements touch a helper at all**. Charging 171
instructions to "each exit" charges them to an event that happens about once in
2 000 iterations.

---

## 5. Which exits trigger the conditional causes, and how often

| conditional cause | n | trigger | frequency, from the counters |
|---|---|---|---|
| dispatcher-return block (row 16) | 59 / 58 / 56 | latch's inv-gen guard fails, or chain depth ≥ 48 at the exit | inv-gen: `code_invs = 3`, `resyncs = 15 540` against `hops = 980 771 813` → ≈ 1.6 × 10⁻⁵ of exits. Depth: no counter; the 48-deep cap, reached only when a chain nest is not popped |
| conditional return arms (row 15) | 11 / 12 / 11 each, ×2 | the same inv-gen mismatch, and the depth cap | as above |
| pack (row 6) | 24 | **every** exit — it is on the spine, not behind a branch, because the successor block reads flags (`needs_flags`) | 1.0 per exit |
| inline chain-table probe (row 11) | 0 / 12 / 0 | #2's exit only, when the table's first probe slot is consulted | per exit on #2 |
| cold legs + untaken copy (row 17) | 174 / 165 / 172 | probe miss, or the guard selecting the other body copy | `load+store = 14 891 787` / 28.99 G retired ≈ 0.05 %; the untaken copy runs 0 times for a given guest state |

Only the pack is genuinely per-exit. Rows 15 and 16 are 80 instructions of
layout that essentially never execute — and they are in the 337 as if they did.

---

## 6. The > 25 % test, and the verdict

25 % of 337 is **84.25 host instructions**. Against that threshold:

* **Row 17, the cold group, 171.1 — above it, and not a slice.** It is not an
  exit cost (0.05 % of retirements), it is not one cause (helper legs *plus* a
  duplicated body), and it is `lower/mem.rs` and the super-path lane's, which
  this round may not touch. Deleting it would remove the fallback the stack
  guard needs when it fails — the copy exists so that a guard failure can fall
  back to the probes.
* **Every executed cause is below it.** Largest executed: row 5 probe-hoist
  arithmetic 44.3 (13.1 % of 337), row 8 writeback+reloads 29.3 (8.7 %), row 3
  spills 25.1 (7.4 %), row 2 loads 24.0 (7.1 %), row 6 pack 24.0 (7.1 %).
  Rows 2+3 are the same 16 values moved twice and are the known
  self-loop-header-params-through-stack-slots mechanism
  (`scaled-blit-block-attribution.md` §12.3: **live phi materialisation, not
  redundant writeback**; `gpr_store_predicate` removes zero of it). Row 5's line
  is closed as spent (§11, §12.2). Row 6's line is the flag census, closed.

**So: no single cause above 25 % of 337. The distributed 337 is not a slice**,
and the census does not name one. Nothing was removed, nothing was measured
against a baseline.

---

## 7. If the denominator is corrected, the shortlist changes

Re-priced against the **executed** bill (197.6 per entry+exit, 26.2 per
iteration) rather than the 337:

1. **GPR load → frame slot (rows 2+3, 49.1) plus the exit-side reloads inside
   row 8 (≈ 11).** 30 % of the executed entry+exit bill, for zero useful work:
   the 16 guest GPRs are loaded from `JitCtx`, spilled to the frame, and
   reloaded to be written straight back. §12.3 already proved the stores are
   live (off-path blocks read those slots) and that this is register pressure on
   17 block params, so the lever is carrying fewer values as block parameters or
   handing the parameter to the successor directly — a block-parameter semantics
   change that needs its own slice and an explicit pessimism/optimism trade.
2. **Probe-hoist arithmetic (row 5, 44.3).** 22 % of the executed bill, but
   §12.2 measured the alternatives as negative: the on-path guard is 3 per
   access, and hoisting more adds work.

Both are larger in the *executed* denominator than anything inside the 337, and
neither is a flag or epilogue change. Quoting 45 per iteration keeps pointing
slices at cold layout; quoting 26.2 points at block-parameter materialisation.

---

# 8. The parameter-materialisation breakdown (what §7's item 1 was made of)

The 49–60 is the block-parameter round trip: the entry loads a guest register out
of `JitCtx`, spills it to the header's home slot, and the shared exit block
stores it back. §8 asks *why each value is there*, from the CLIF (which register
each param is, whether the loop reads it, whether the loop writes it) and the
emitted AArch64 (which of those values actually gets a load, a spill, a store and
a reload). Instrumented on one release build of this tree, one bounded DOOM
Retro run per side (`WIE_JIT_MAP` + `WIE_JIT_DUMP_IR` + `WIE_JIT_REGION=0`),
blocks disassembled with `lldb -p`.

## Counting rule

Per guest register slot (16 GPRs + rflags), four instruction kinds belong to the
parameter round trip: one entry load, one entry spill (if regalloc gives it a
home), one exit store, one exit reload (if the stored value has to be re-read out
of its home). The whole **unit** also contains frame setup, probe-hoist
arithmetic, the pack, RIP, trip flush and teardown; those are §2's rows 1, 5, 6,
7, 13, 14 and are excluded here.

A "read" is a use of the header param in the loop body that is not merely a
`store` of that value straight back into `JitCtx`. That distinction is what makes
the census honest: the *cold* writeback arms (inv-gen stale, chain-depth cap,
dispatcher return) touch every param, and counting those as reads would hide the
whole class.

| | #1 0x1800d8fb0 | #2 0x1800a1c60 | #3 0x1800cef0b |
|---|---|---|---|
| entry loads (16 GPR + rflags) | 17 | 17 | 17 |
| entry spills | 15 | 14 | 14 |
| entry register shuffles | 1 | 1 | 0 |
| exit stores (16 GPR + rflags) | 17 | 17 | 17 |
| exit reloads | 4 | 3 | 1 |
| **total** | **54** | **52** | **49** |

Weighted by leaf share (0.563 / 0.285 / 0.152): **52.6** — the 49–60 range, at
its low end.

## The five classes

| class | why the value is there | #1 | #2 | #3 | weighted | share |
|---|---|---|---|---|---|---|
| **(i)** | the loop never reads it — dead on entry | 33 | 39 | 40 | **35.8** | **68 %** |
| (ii) | the loop reads it, but it is never written, so `JitCtx` already holds it | 2 | 0 | 1 | 1.3 | 2 % |
| (iii) | spill → reload of the same slot with nothing writing it in between | 4 | 0 | 0 | 2.3 | 4 % |
| (iv) | the flag word, carried only to satisfy the param list | 1 | 1 | 1 | 1.0 | 2 % |
| (v) | read **and** written in the loop — genuinely loop-carried | 14 | 12 | 7 | 12.4 | 24 % |

Class (i) decomposes into two sub-shapes, and only the first is free to delete:

* **never read, never written** — 10 / 12 / 13 registers (`rbx, rsp, rbp, rsi,
  rdi, r11..r15` on #1; `rcx, rbx, rsp, rbp, rsi, rdi, r10..r15` on #2;
  `rcx, rdx, rbx, rsp, rbp, rsi, rdi, r9, r10, r12..r15` on #3). Their load,
  spill, store and reload are all removable.
* **written but never read** — `rcx` on #1, `rax` on #2. The load is dead; the
  *store* is required, and `gpr_dirty` keeps it even when the register is not a
  param.

What the loop actually reads: #1 five GPRs (`rax, rdx, r8, r9, r10`), #2 three
(`rdx, r8, r9`), #3 three (`rax, r8, r11`). Sixteen guest registers are carried
for five, three and three.

(iv) is one instruction per block — the `rflags` load. It is *not* the flag
construction (that is the pack, §2 row 6, which is per-exit and stays): the word
is loaded once so `FlagState` keeps the bits it does not track. The flag lane is
untouched here.

## 9. What was removed

The widening itself, and the store that existed only to match it. Two hunks, both
in `lower/mod.rs`:

* `live_eff.fill(true)` for self-loops is gone: the header carries the live-in set
  (`analyze_live_gprs` + the fast-call extras + the stack-guard base), which is
  the same set every non-self-loop block already used.
* the shared exit block's store predicate loses `|| self_loop`, so a self-loop
  writes back exactly what it loaded, like every other block.

**Why the live set is enough for a loop-carried value.** `analyze_live_gprs` marks
a register when the block *mentions* it — any operand, any memory base or index,
any `opclass::implicit_gprs` row — so a register read before it is written in the
pass is always a param, which is exactly the loop-carried case. A register written
before it is read does not need the incoming value.

**Why the loaded mask is enough for the store.** Every writer mentions its
destination, so "written ⇒ mentioned ⇒ live ⇒ loaded at entry". The mask is
therefore a superset of "defined", and a register that is neither is not stored —
which is what keeps the `iconst 0` placeholder from zeroing live guest
registers (the 7za failure the old comment described for `dual_super`).

**No pessimism trade.** For `self_loop == false` both edits are no-ops: `live_eff`
was already `live`, and the removed disjunct was false. No block gains a param,
so no block that took the leaf path can need a frame now. The three blocks above
are the only ones this changes in the measured set.

## 10. Acceptance, in the census's own unit

Same instrument, same three blocks, one release build per side, one bounded run
each. "Param traffic" is §8's total; "latch" is the back edge's frame stores +
loads (the phi materialisation §12.3 described); "hot path" is the executed
loop-body instruction count (head ∩ co-reachable-to-the-back-edge).

| | #1 before → after | #2 before → after | #3 before → after | weighted |
|---|---|---|---|---|
| entry | 112 → 94 | 109 → 81 | 88 → 54 | |
| param traffic per entry+exit | **54 → 19** | **52 → 10** | **49 → 9** | **52.6 → 14.9** |
| — of which per iteration (÷ 7.546) | 7.2 → 2.5 | 6.9 → 1.3 | 6.5 → 1.2 | **7.0 → 2.0** |
| latch frame traffic per iteration | 16 → 6 | 15 → 3 | 15 → 2 | **15.6 → 4.5** |
| hot path per iteration | 238 → 228 | 211 → 109 | 333 → 288 | **244.7 → 203.3** |
| unit | 787 → 608 | 668 → 528 | 757 → 613 | |

**−37.7 host instructions per entry+exit (−72 %), −5.0 of that per iteration at
hops/entries = 7.546, and −11.1 more per iteration in the latch.** The executed
loop body itself fell by 41.4 weighted instructions per iteration (−16.9 %): with
four to seven live params instead of seventeen, the body no longer spills guest
values to the frame at all (#2: 21 frame stores → 0, 44 frame reloads → 15).

That the drop is *only* materialisation is checked at the CLIF level: between the
before and after dump of the same block, every guest-visible opcode count is
identical (`icmp` 50/50 on #2, `band`, `bxor`, `ishl`, `ushr`, `isub`,
`select`, `imul`, `call`, `uextend`, `bor`, `brif` and `return` all unchanged)
and only `iconst`, `iadd`, `load` and `store` move — the ctx-address
materialisation and the parameter loads/stores. #1: 905 → 785 CLIF ops; #3: 986 →
830, same signature.

Residual, deliberately not taken: a register that is written and never read still
gets an entry load, because `analyze_live_gprs` cannot tell reads from writes
(`rcx` on #1, `rax` on #2 — one instruction each per entry). Splitting the
analysis would remove it; it is not worth a new liveness notion for two
instructions.

