# Region-level register allocation for guest GPRs

**Verdict: drop.** The region's per-iteration `JitCtx` GPR-slot traffic is already
zero — measured, in CLIF and in machine code — so pinning the loop's guest GPRs
into fixed host registers has a measured ceiling of **0 host instructions per
iteration, 0.0 % of the ~950-instruction iteration body**. The remaining
per-iteration stack traffic is the register allocator's own spill traffic, and
AArch64 offers **10** callee-saved GPRs under Cranelift's `apple_aarch64` ABI,
not 19, so the 16-guest-GPR design in the brief cannot be built and a partial
version of it cannot reach the spills either.

Date: 2026-11-24. Repo `/Users/yanis/Programming/wie`, HEAD `2176d8d`
(`WIE_JIT_REGION=1`, off by default). Host: macOS 26.5.2, Apple Silicon, 8 cores,
release build of `target/release/wie`, `opt_level=none`.
Every number below was measured in this session on this machine; anything
estimated or unmeasured is labelled. CLIF op counts are used **only** where a
1:1 host-instruction correspondence is verified (§2) — never as a quality metric.

---

## 1. The ceiling, before any design

### 1.1 What the region actually emits

Two sources, and they agree to the instruction.

**CLIF.** `WIE_JIT_DUMP_IR=/tmp/region.clif WIE_JIT_DUMP_IR_RIP=0x140066570`
(`WIE_JIT_DUMP_IR` takes a *path*, not `1`) against the emission test that loads
the real PE bytes — a live run does reach the loop on this host, but the test
gives a deterministic dump in 60 ms:

```sh
WIE_JIT_DUMP_IR=/tmp/region.clif WIE_JIT_DUMP_IR_RIP=0x140066570 \
  cargo test -p wie-cpu --lib doom_hot_loop_emits_one_function -- --nocapture
# doom region (0x140066570): clif blocks=69 clif_insts=1562 guest_insns=36
#                           live_in=7 exits=2 back_edges=1
```

Counting every `load`/`store` whose address resolves (through the SSA defs and
`iadd`/`iadd_imm` chains, block parameters opaque) to `ctx_ptr + k`:

| site | GPR slots (`k` = 0…120) | other ctx fields |
| --- | --- | --- |
| entry block (`block0`) | **16 loads**, one per slot, once | rflags@128 + 7 pin/refill loads |
| region block 0 (header) | 0 | 0 |
| region block 1 | 0 | 0 |
| back-edge landing block | 0 | 0 |
| 2 exit blocks + shared fault/return + invalidation bail-out | **64 stores** (4 × 16) | rip, insn_acc, chain counters |
| **per region iteration** | **0** | **0** |

Distinct GPR slots touched inside an iteration: **0**. Re-loads of a slot already
loaded in the same iteration with no intervening store: **0**. There is nothing
to remove.

**Machine code**, from a live run — `WIE_JIT_REGION=1 WIE_JIT_MAP=/tmp/rmap.txt`,
then `lldb -p <pid> -o 'disassemble --start-address 0x132808000 --count 1500'`
on the map's host address for `0x140066570`:

| | DOOM region `0x140066570` |
| --- | --- |
| function size | 1304 host instructions, 5216 B (`0x132808000`–`0x13280945c`) |
| entry | 16 × `ldr xd, [x0, #0…0x78]`, then 12 go straight to a stack slot |
| iteration body (`header` … back edge) | 997 static instructions; 956 reachable from the header with the 3 exit blocks excluded; 931 excluding blocks that pass through a `blr` |
| GPR-slot accesses in the body | **0** |
| `ctx`-relative accesses in the body | 0 — the probe's pin/refill/gen pointers are hoisted at entry (`add x24, x0, #off`) |
| `[sp]` accesses in the body | 80 (65 reloads, 15 stores) over 20 distinct slots |
| back edge | 8 instructions: 4 `stur`, 2 `mov`, 1 `ldur`, `b header` |
| guest GPRs that survive the prologue in a register | 4 of 16 (12 spilled immediately) |

The 16 entry loads and four 16-store writeback sites (at `0x132808dcc`,
`0x132809094`, `0x132809158`, `0x13280932c`) are exactly what the CLIF says.
A naive scan reports two extra "slot 0" hits at `0x132808e3c`, which are the
`ldar` invalidation-generation load aliasing `x0`; they are not GPR traffic.

### 1.2 The block path for the *same* guest code, re-measured today

From a live run with regions off, map address → `lldb`
(blocks `0x140066570` = 25 guest insns, `0x1400665c4` = 11):

| | block `0x140066570` | block `0x1400665c4` | per iteration |
| --- | --- | --- | --- |
| host instructions | 968 (3872 B) | 436 (1744 B) | **1404** |
| `JitCtx` GPR-slot accesses | 42 (12 loads, 30 stores) | 26 (8 loads, 18 stores) | **68** |
| distinct slots | 11 | 8 | ≤19 |
| busiest slot | RAX, loaded 2× | five slots 2× | 1 redundant re-load |

Cross-check: the same disassembly reproduces the parity doc's census for the
25-instruction block (measured **42 `ldp` / 43 `ldur` / 52 `str` / 19 `stur`**,
doc says 42 / 45 / 52 / 17), so the tooling and the earlier audit agree. The
brief's "63 redundant accesses over 29 slots, one slot loaded 14×" is the right
order of magnitude for a hop (68 measured here) but is **not** what these two
blocks do: neither has a slot loaded more than twice. The 14× figure belongs to
a different, larger block.

### 1.3 The premise this refutes

The brief states that inside a region "each guest GPR access is a load from or
store to the `JitCtx` frame slot at a fixed offset, not a host register". That is
the **block** path's contract. The region emitter
(`lower/region_emit.rs`, module docs points 1–2) threads guest state as
`[i64; 16]` **block parameters** plus the RFLAGS carrier on every internal edge,
and `STATE_PARAMS = 17`. `JitCtx`'s GPR slots are therefore touched exactly
twice in a region's lifetime per entry: 16 loads at entry, 16 stores at each of
the four writeback sites. There is no per-block frame-slot traffic left to
delete — SSA block parameters already deleted it, and that is where the region's
win came from: 1404 → ~931–956 host instructions per iteration (−32 %), of which
only 68 instructions (4.8 % of the block path) were ever frame traffic.

Two side effects of the current shape are worth naming, because they are what a
"register allocation" round would actually be fixing:

* **All 16 GPRs are loaded at entry regardless of `live_in()`** (7 here), costing
  9 dead loads per *region entry* — once per entry, not per iteration.
* **The 17-value back-edge transfer costs 8 instructions per iteration**
  (4 stores + 2 moves + 1 reload + branch, §1.1) — 0.8 %.

---

## 2. Conversion to host instructions

At `opt_level=none` the AArch64 backend folds `iadd_imm(ctx, k)` + `load`/`store`
into a displacement-form access, so **one CLIF load/store op is one host
instruction** and one CLIF *address computation* is zero. Verified rather than
assumed: the 16 entry loads appear as 16 `ldr xd, [x0, #imm]` with no `iadd` in
between, and the 80 body `[sp]` ops are 80 `ldur`/`stur`/`ldr`/`str`.

* **(a) Host instructions saved per region iteration by pinning: 0.**
  There is no per-iteration frame-slot load or store to convert.
* **(b) Fraction of emitted host instructions per iteration: 0 / 931–956 =
  0.0 %.** (Static body 997; 956 excluding exit blocks; 931 excluding blocks that
  reach a `blr`; the band is the honest range because both sides of every
  conditional are counted as reachable.)

For completeness, the two quantities that *are* non-zero per iteration and that a
register-allocation round might legitimately target:

| per iteration | DOOM region | share of 931–956 |
| --- | --- | --- |
| `JitCtx` GPR-slot accesses | 0 | 0.0 % |
| back-edge 17-value transfer | 8 | 0.8 % |
| `[sp]` spill/reload (allocator's own) | 80 | 8.4–8.6 % |
| `cmp`/`tst`/`csel`/`cset`-class ops (packed-flags work) | 206 | 21.5 % |

---

## 3. The design, against the constraints that actually bind

**Register budget — the brief's premise is wrong.** Cranelift's `apple_aarch64`
ABI gives us **10** callee-saved GPRs (`x19`–`x28`); `x29` is the frame pointer
and `x30` the link register, both saved/restored by the prologue; `x0`–`x18` are
caller-saved. The region function's own prologue confirms it: `stp x19, x20` …
`stp x27, x28`, five pairs, ten registers. "19 usable callee-saved GPRs" would be
the count of `x0`–`x18`, which survive a *region* body only until the first
helper call.

So **16 guest GPRs do not fit** — not in 10, and not in 10 minus what the body
needs besides them:

| claim on the 10 | count |
| --- | --- |
| JitCtx base | 1 (already in `x0` on entry; the emitter also copies it to `x9`/`x28`) |
| scratch for the TLB probe / refill compare / flag materialisation | 1–2 |
| **pinned guest GPRs, in practice** | **7–8** |

Which registers, by what rule: `x19`–`x26` for the first eight entries of
`live_in()` in `reg_index` order, `x27`/`x28` reserved as the probe/exit scratch
pair, `x0` as the base. Seven is also exactly DOOM's live-in count
(`RBX RBP RSI RDI R11 R14 R15`), which is the only version of this design that
fits at all: **pin `live_in()`, not the file**. The other nine GPRs are written
before read inside the region, so their entry values are dead; they only need a
home for the exit writeback, where a stack slot is sufficient.

**Spill/reload at helper calls.** The point of callee-saved homes is that a
pinned value survives a `bl` — and the region body's three `blr` sites
(`0x1328081e4`, `0x132808340`, `0x132808564`: probe miss paths) plus the exit
path's `wie_jit_chain_lookup` calls are precisely why regalloc spills today:
17 values live across a call site, 10 callee-saved registers, therefore ~7 in
memory for the whole body. Pinning 7 guest GPRs into `x19`–`x26` leaves 3
registers for *every* temp live across those calls, so the body's 65 reloads do
not disappear — they change owner. There is no spill state to keep "correct" at a
helper call beyond what exists today: the pinned registers are callee-saved, so
the ABI already preserves them, and the argument registers (`x0`–`x7`) carry the
helper's arguments, not guest state.

**What an exit stores under pinning.** Still 16 stores. The exit must write the
*architectural* file because `JitCtx` is the only copy the next compilation unit
or the interpreter can see (`region_emit.rs`, "What an exit stores, and why that
is sufficient"), and that does not depend on where the value lived inside the
region. Pinning changes the *source* of each store (a fixed register instead of a
block-parameter value); it does not change the count, and the earlier idea of
narrowing it belongs to a live-out analysis, not to this design. The same holds
for the 4 writeback sites: exits are end-of-loop events, not per-iteration work,
so even halving them buys ~0 per iteration.

**What entry loads under pinning.** With `live_in()` narrowing, 7 loads instead
of 16 and no block-parameter copies — a saving of 9 loads **once per region
entry**. That is a real but entry-scoped win, and it is available from a
five-line change to `STATE_PARAMS` without touching register assignment at all
(§4).

**RFLAGS carrier.** Unchanged. It is already one SSA value carried on the back
edge and decomposed/re-packed per region block; pinning the 7 GPRs does not touch
it, and it would occupy one of the "7–8" slots if it were pinned too, which is
another reason not to bother.

**Invalidation vs a stale pinned guest value.** No new hazard, and no new
mechanism needed. The back edge already re-checks `inv_gen` against the baked
generation (one atomic load, one compare, one branch) and bails to a full
writeback + `rip = region entry` + `return`; a pinned register holds exactly the
value the SSA parameter holds today, so the guard covers it. The one thing a
pinning implementation must not do is keep a pinned register *across* region
entries — `JitCtx` is stale at entry by construction, and the entry load is what
makes it fresh. The bail-out path must read the pinned registers in the same
order as the block-parameter path; it is the same 16 stores either way.

**Implementation cost, and why it is not a small diff.** Cranelift has no
"pin this `Value` to `x19`". The options are (a) a custom `TargetIsa` ABI whose
signature assigns specific registers to the state parameters — Cranelift's
argument machinery maps parameters to the *fixed* argument sequence (`x0`–`x7`
then stack), not to a chosen callee-saved set, so this means forking its ABI
plumbing; or (b) emitting the region body as hand-written AArch64 through
`MachBuffer`, which abandons `lower_insn`, the shared `FlagState` lowering and
the refill probe for every guest instruction in every region. Either way this is
a second emitter or a fork of the ABI layer, to serve 7 registers that regalloc
already prefers for loop-carried values.

---

## 4. Cost and worth

**Compile time: not measured.** The run profile carries `compile_us` and
`compile_by_insns` and a paired `WIE_JIT_REGION=0/1` A/B would give it, but I did
not run it, and with a 0 % ceiling there is nothing for it to justify. Bounding
shape only: the region is one `define_function` of 69 CLIF blocks / 1562 ops
(reported by the emission test above) where the block path is two functions
carrying the same 36 instructions.

**Code size: measured, and the region is already smaller.**

| | host instructions | bytes |
| --- | --- | --- |
| region (one function) | 1304 | 5216 |
| block path (two functions) | 1404 | 5616 |

Pinning would not move this materially: it replaces block-parameter copies with
fixed homes, and today's back-edge transfer is only 8 instructions. A small
region does cost more than its blocks would (`0x1400aced0`, 8 guest instructions,
compiles to 700 instructions / 2800 B), but that is region formation's cost, not
pinning's.

**Register-light loop: DOOM is the best case, not the typical case.** Second
region, guest `0x1400aced0` (8 guest instructions, two blocks, ~4 GPRs read
before written — `R9`, `R10`, `R11`, `RCX`, read by hand from the guest bytes;
formation's exact set was not re-run for this VA), same method:

| | region `0x140066570` (7 live-ins) | region `0x1400aced0` (~4 live-ins) |
| --- | --- | --- |
| guest instructions | 36 | 8 |
| body, static / reachable / without `blr` | 997 / 956 / 931 | 623 / 535 / 506 |
| `JitCtx` GPR-slot accesses per iteration | 0 | 0 |
| `[sp]` spill traffic | 80 = 8.4 % | 75 = 14.8 % |
| guest GPRs surviving the prologue in a register | 4 of 16 | 2 of 16 |
| packed-flags-class ops | 206 = 21.5 % | 104 = 19.4 % |

The spill share goes *up* as the loop gets smaller, because the region threads a
fixed 17-value state regardless of `live_in()`. So the loop that would benefit
most from pinning is the one where pinning has the fewest live-ins to pin, and
the loop with the most to pin (DOOM, 7) is the one that amortises the state cost
best. There is no regime in which this pays.

---

## 5. Verdict

**Drop.** The measured ceiling is 0 host instructions per region iteration
(0.0 % of the 931–956-instruction body): the region already keeps guest GPRs in
SSA block parameters and touches `JitCtx`'s GPR slots only at entry and exit, so
fixed host registers would have nothing to remove — and AArch64 gives 10
callee-saved registers, not 19, so the 16-register design cannot be built anyway.
The campaign's remaining budget is better spent on the ~21–38 % of hot-block
machine code that is EFLAGS construction.

**Cheaper items this measurement does support**, in order, none of them pinning:

1. **Narrow `STATE_PARAMS` from 17 to `live_in() ∪ written`** in
   `region_emit.rs` (§3, §1.3): removes 9 dead entry loads per region entry and
   thins the back-edge transfer. Emitter-only, no ABI work, no compile cost.
2. **Narrow the exit writeback** to the live-out that can reach the exit — the
   64 stores are already outside the iteration, so this is a code-size and
   exit-cost item, not an iteration-cost item.
3. **EFLAGS**: 206 of 956 body instructions (21.5 %) are `cmp`/`tst`/`csel`/
   `cset`-class, consistent with the parity doc's 38 % finding for the block path.
   This is the largest per-iteration item left in the region body.

**What would change this verdict:** a measurement showing non-zero per-iteration
`JitCtx` GPR traffic in a region (i.e. the emitter regressing to block-path
semantics), or a target where the callee-saved set is ≥16 and regalloc still
spills loop-carried guest state. Neither is the case here.

---

## Appendix: reproducing the numbers

```sh
# CLIF of the region
WIE_JIT_DUMP_IR=/tmp/region.clif WIE_JIT_DUMP_IR_RIP=0x140066570 \
  cargo test -p wie-cpu --lib doom_hot_loop_emits_one_function -- --nocapture

# live run, regions on, with a host-address map
WIE_JIT_REGION=1 WIE_JIT_MAP=/tmp/rmap.txt \
  ./target/release/wie run --bottle doomretro --gui doomretro.exe -- \
    -iwad 'C:\Program Files\DoomRetro\freedoom2.wad'
grep -E ' 0x140066570 ' /tmp/rmap.txt            # -> insns=36 (a region, not a block)

# machine code of that region, and of the block path with regions off
lldb -p <pid> --batch -o 'disassemble --start-address <host> --count 1500' \
  -o detach -o quit
```

Counting rules used (all mechanical, no eyeballing):

* GPR slot = `ctx_ptr + 8·i`, `i` in 0…15 (`JitCtx::gpr` at offset 0, `rflags`
  at 128, `rip` at 136). In CLIF, addresses are resolved through the SSA defs of
  `iadd`/`iadd_imm`/`iconst` chains with block parameters treated as opaque; in
  AArch64, through a linear copy-propagation of `x0` (the context parameter) into
  `x9`/`x28`.
* "Iteration body" = the code between the loop header and the single backward
  branch (`b 0x132808100` at `0x132809090`), excluding blocks that end in `retab`
  (the exits) and, for the narrower figure, blocks that pass through a `blr`.
