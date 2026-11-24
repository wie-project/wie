# The chain hop: what an edge costs, and where it still leaves emitted code

Scope: the block-to-block transfer emitted by
`crates/wie-cpu/src/jit/lower/emit.rs::emit_chain_or_exit`, and the Rust paths
that survive it. ADR-0002 holds the design history (the direct-register hand-off
is still blocked; the hop is still a nested host C ABI call). This file is the
measurement and the residual-cost ledger.

## Shape per hop, in emission order

| # | What | Emitted on |
| --- | --- | --- |
| 1 | `ctx.rip = target` | every edge |
| 2 | GPR writeback, `dirty \|\| loaded` (≤16 stores) | every edge |
| 3 | invalidation guard: `*inv_gen_ptr == ctx.inv_gen_baked`, else dispatcher | every edge when chaining is on |
| 4 | `chain_depth >= MAX_CHAIN_DEPTH` → dispatcher (RIP already advanced) | every edge |
| 5 | `chain_depth += 1` | every edge |
| 6 | tier 0: direct `call` to a same-module successor | same-tier, earlier-compiled target |
| 7 | tier 1: edge IC, 4 slots, one `brif` per slot | otherwise |
| 8 | tier 2: chain table slots `hash(target)` and `hash(target)+1`, inline | after tier 1 |
| 9 | tier 3: `wie_jit_chain_lookup(ctx, target)` | after tier 2 |
| 10 | shared transfer block: hop accounting, `call_indirect`, `chain_depth -= 1`, trip flush, `return` | every resolved hop |

## Per-hop CLIF op count (`WIE_JIT_DUMP_IR_RIP`, one ring block, `opt_level=none`)

`micro-exes/chain_hops`, block at `rip=0x140001058` (a 4-instruction guest
block), counting CLIF instructions on the executed path only:

| Region | Before | After |
| --- | --- | --- |
| hop prologue in the entry block (`rip` store, 5-slot writeback, inv guard) | 26 | 26 |
| depth-guard block | 6 | 6 |
| successor resolution | **66** (edge IC, `select` cascade, both initialisers) | **14** (tier-1 slot 0) … 47 (tier-1 slot 3) |
| transfer block (accounting + `call_indirect` + depth restore + flush) | 20 | 20, and one copy instead of three |
| **total per hop, tier-1 slot 0** | **118** | **66** |
| **total per hop, tier-1 slot 3** | **118** | **99** |

The tier-2 probe adds 21 ops (`14` + `7`) but is only reached when tier 1 misses;
before, the same miss went straight into the Rust resolver — a host call plus its
own 4-slot IC scan and 16-step linear probe. So the new shape is never worse, and
the more often the IC misses the more it wins.

These are CLIF instructions at `opt_level=none`, where the emitter is the
optimiser and a redundant `band` is a real host instruction, so they are a proxy
for host instructions, not a measurement of them. They are deterministic and
reproducible from the dump; wall-clock numbers are not (three concurrent LTO
builds).

## How much still leaves emitted code

`JitCtx::chain_helper_calls` counts entries to
`lower::mem::wie_jit_chain_lookup`, i.e. hops that neither inline tier answered.
On `micro-exes/chain_hops` (40 000 double-block iterations): **2 resolver calls
per 39 182 hops**. With the edge IC wiped before every dispatcher entry (tier 1
forced to miss, `chain_hop_resolves_from_the_chain_table_with_a_cold_edge_ic`):
3 917 of 4 000 hops still transfer natively from tier 2.

The Rust trampoline is still reached in exactly these cases:

1. **Depth cap** — `chain_depth >= MAX_CHAIN_DEPTH`: return to the dispatcher
   (not a resolver call; the frame is re-entered from Rust with RIP advanced).
2. **Invalidation** — the generation moved past the frame's bake: return to the
   dispatcher for purge and re-decode.
3. **Lookup miss** — tier 1 and tier 2 both missed and the resolver returned 0
   (target not compiled, or displaced past the second probe slot): dispatcher.
4. **Resolver hit after two misses** — a table slot displaced further than one
   step, or a cold edge IC: one host call, then a native transfer.
5. **Micro-stub successors** — a guest block that matched a hand-written stub
   (`ret`, `xor eax,eax; ret`, `mov eax,imm32; ret`, `mov rax,rcx; ret`,
   `mov eax,[gs:0x68]; ret`, `mov [gs:0x68],ecx; ret`) *is* Rust, and its chain
   tail is `trampolines::chain_tail`. That path is unchanged.
6. **Fault set** — chaining stops; RIP is already on the faulting successor.

## Why the per-hop writeback is not narrowed per successor

The proposal was to store the successor's live-in masks beside its code pointer
and write back only those. Two problems, one fatal:

- **Wrong set.** The dispatcher's `gpr_dirty_bits` writeback needs the registers
  the successor's native code *writes*, not the ones it *reads*. Live-ins are
  the other half of the hand-off and are already covered by the emitted
  predicate.
- **Not expressible per slot (fatal).** A slot describes one target, but the
  written set is transitive over the chain that target starts: a stub target can
  chain to a Cranelift block that writes `rbx`, and no per-slot mask knows that.
  A correct per-slot mask is a fixpoint over the reachable block graph, which the
  table does not have (blocks arrive in any order, from other threads, and the
  table is cleared on every invalidation).

The only sound cheap alternative is an accumulator: every block `or`s its own
write mask into `gpr_dirty_bits` on entry (3 CLIF ops per block execution) and
`chain_tail` stops forcing `ALL_DIRTY`. It buys one 16-register sync per
*dispatcher entry* (≤ 48 hops) and costs 3 ops per *hop*, so it is negative on
this workload, and it widens the correctness surface of the dispatcher's
writeback to "every emitted store to `ctx.gpr` must also record its mask". Not
taken.

## Counters

- `JitStats::chain.hops` / `chain.store_ops` — transferred edges and the GPR
  slots their writeback touched. Emitted-side only.
- `JitCtx::chain_helper_calls` — resolver entries. Cumulative per engine (not
  reset per dispatch entry); not yet folded into `JitStats`, so it is read
  directly from `PerThreadJitState::ctx`. Wiring it into the `jit_chain` profile
  line is one fold in `run_compiled` plus one field in `JitStats`/`diag`, both
  outside this change's lane.
