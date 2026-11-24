# ADR 0010 — Direct block-to-block register hand-off: second feasibility pass

Status: **Blocked** for the direct-register ABI (external constraint, named below) ·
**Feasible, proceed** for the scoped partial win in [Decision](#decision) ·
2026-10-03 — supersedes the open half of [ADR 0002](0002-direct-register-abi.md)

## Context

[ADR 0002](0002-direct-register-abi.md) proposed keeping guest GPRs in native
callee-saved registers (`x19–x28` = `rax…r15`, `x14` = rflags carrier) across a
block chain, so a chained successor does not re-cross the `JitCtx` boundary. It
budgeted the removable cost at **~15 ms per 100 M guest instructions** (750 k
block transitions), on the theory that the load/store traffic steals memory
bandwidth from the pooled surface blits.

Its 2026-09-05 feasibility pass concluded the ABI is not expressible through
Cranelift and proposed a staged path: (1) tail chaining, (2) a hand-written
per-block prologue stub, (3) SSA rflags at the stub boundary. Its 2026-09-25
status note then deleted `WIE_JIT_DIRECT_REGS` and `WIE_JIT_TAILCHAIN`, because
neither gated a lowering call site. Nothing has been built since.

This pass re-derives the answer against the **resolved** Cranelift version, and
separately asks whether a partial win exists that does not need the ABI.

### Three facts that reframe the question

**F1 — The perf gate has zero block transitions in it.** `long_loop` is one
basic block. Disassembly of `micro-exes/out/long_loop.exe` shows the entire
100 M-iteration loop is `0x140001040 … 0x140001075` ending in
`jb 0x140001040` — a single self-loop. A live profile confirms it:

```
block_entries=1 insn_per_entry=1100000014.0  jit: insns=1100000011 compiles=2
```

Self-loops already pass live GPRs and rflags as **SSA block params**
(`lower/mod.rs:1081-1118`, `:1201-1234`), so a self-loop iteration costs *zero*
`JitCtx` GPR traffic. `docs/status.md:40` already records this
("`long_loop` is `block_entries=1` … 1.1e9 instructions retired inside one
dispatcher entry"). Therefore **the ~15 ms budget is, by construction, invisible
to `scripts/check.sh --only perf`**, which gates on `long_loop` alone. Any
success criterion written against `long_loop` — including ADR 0002's own
"`long_loop` 0.25 s → ≤0.10 s" — is arithmetically unreachable.

**F2 — The 15 ms figure has no live instrumentation.** `ExecStats`
(`jit/mod.rs:133-142`) carries `jit_insns`, `iced_insns`, `cache_hits`,
`code_invs` — there is **no per-edge chain-hop counter**. The "750 k
transitions" number cannot be reproduced from the current tree. `jit_chain
avg_width` is *not* it: `avg_width = resync_entries / resyncs`
(`jit/diag.rs:92-102`) measures chain-**table population**, not chain-hit rate.

**F3 — The 15 ms is small against the wall it competes with.** 7-Zip Extra warm
`emu_ms` is ~989 ms at the current default (`docs/status.md:38`), and 259–679 ms
in the verifier A/B (`docs/status.md:42`). The whole boundary cost — GPR
round-trip, IC probe, `chain_depth` bookkeeping, `call`/`ret` — is therefore
**~1.5–6 % of `emu_ms`**. The GPR share of that is a fraction of the fraction.

Taken together: the lever is real but small, and the gate that would judge it
cannot see it.

## The six questions

### Q1 — Has the Cranelift surface changed?

**Resolved version: `cranelift`, `cranelift-codegen`, `cranelift-jit`,
`cranelift-module`, `cranelift-native` all at `0.133.1`** (`Cargo.lock:473-648`).
`crates/wie-cpu/Cargo.toml:22-26` pins `"0.133"`. **Unchanged**, so every
constraint in ADR 0002 still holds — with one correction and one important
refinement.

**(a) The 17-result constraint still holds, verbatim.** On aarch64,
`max_per_class_reg_vals = 8` (x0–x7) and `remaining_reg_vals = 16`
(`cranelift-codegen-0.133.1/src/isa/aarch64/abi.rs:200-201`). A parameter is
pushed into a register only while `*next_reg < max_per_class_reg_vals &&
remaining_reg_vals > 0` (`abi.rs:352-353`); otherwise, for returns,
`abi.rs:380-385` returns
`CodegenError::Unsupported("Too many return values to fit in registers. Use a
StructReturn argument instead. (#9510)")`. 17 I64 returns fail both tests.

**Refinement — the constraint is bypassable, and the bypass defeats the purpose.**
`enable_multi_ret_implicit_sret` (default `false`, `src/settings.rs:508`;
documented at `cranelift-codegen-meta-0.133.1/src/shared/settings.rs:183-200`)
lets excess returns go through an implicit stack return area instead of erroring.
That makes a 17-result signature *compile*, but the values then live on the
stack — the exact round-trip being removed — and the flag is explicitly marked
non-conformant and deprecated. It also cannot be combined with chaining:
`gen_call_args` does
`self.ret_area_ptr.expect("if the tail callee has a return pointer, then the tail
caller must as well")` (`src/machinst/abi.rs:1931-1934`), and a function with no
stack returns has `ret_area_ptr == None`. So this is not an unblock.

**(b) No custom calling convention.** `CallConv` is a closed 8-variant enum
(`src/isa/call_conv.rs:14-59`), not `#[non_exhaustive]`, with no registration
hook. Confirmed by grep: no `custom_abi` / `call_conv_hook` anywhere in
`cranelift-codegen`.

**(c) `FuncEnv` does not exist.** `grep -rn "FuncEnvironment\|FuncEnv"
cranelift-codegen-0.133.1/src/` returns **0 hits**. There is no environment
trait to hang an ABI off, in this or any other sense.

**(d) No way to bind a CLIF value to a fixed physical register.** This is the
finding that matters, and ADR 0002 did not have it:

- `AbiParam` has exactly three fields — `value_type`, `purpose`, `extension`
  (`src/ir/extfunc.rs:139-146`). There is no register field.
- `MachineEnv` is reached only through
  `M::get_machine_env(&self.flags, self.call_conv)`
  (`src/machinst/abi.rs:1542-1544`), and the aarch64 implementation **ignores
  `call_conv`** (`src/isa/aarch64/abi.rs:1179-1188`): it returns one of two
  process-wide `static MACHINE_ENV`s selected solely by the
  `enable_pinned_reg` flag. There is no per-function, per-signature or
  per-call-site override.
- `enable_pinned_reg` pins **exactly one** register: `PINNED_REG: u8 = 21`
  (`src/isa/aarch64/inst/regs.rs:19`), x21.
- regalloc2 0.15.1 (`Cargo.lock:2182-2184`) supports register hints, but
  Cranelift passes none: `grep -n "hint\|Hint" src/machinst/compile.rs` → 0 hits.

**(e) Correction to ADR 0002: tail chaining is *not* blocked by Cranelift.**
ADR 0002 recorded that `return_call` "is rejected by Cranelift 0.133's verifier
under every ABI convention the block signature can use". The verifier statement
is true but the conclusion is too strong. Three facts:

- `CallConv::Tail` **is** implemented end-to-end on aarch64. `ReturnCallInd`
  lowers to `Inst::ReturnCallInd` (`src/isa/aarch64/inst/emit.rs:3061-3075`) and
  emits `Inst::IndirectBr` — a bare `br`, **with no `ret` after it**. The frame
  really is reused.
- The verifier's only convention check on calls is in `typecheck_tail_call`
  (`src/verifier/mod.rs:1713-1736`), which requires (i)
  `cc.supports_tail_calls()` — true only for `CallConv::Tail`
  (`src/isa/call_conv.rs:88-93`) — and (ii) `cc == self.func.signature.call_conv`.
  Plain `call`/`call_indirect` are **not** convention-checked at all.
- The entry-ABI mismatch ADR 0002 treated as fatal is **two instructions**.
  `CallConv::Tail` starts its argument window at `next_xreg = 2`
  (`src/isa/aarch64/abi.rs:178-190` — x0 reserved for the return-area pointer,
  x1 reserved for the indirect-call target), so a one-argument Tail function
  reads its argument from **x2** where `AppleAarch64` reads it from **x0**. With
  ≤8 register arguments `tail_args_size == 0`, so the callee-pops-args path at
  `abi.rs:716` never fires; callee-saves are the same x19–x28 set
  (`abi.rs:1385-1412`). The *only* difference for the 1-argument block signature
  is x0 vs x2.

So: make every block `CallConv::Tail` and add a 2-instruction entry stub, and
`return_call_indirect` becomes legal and correct. **But see Q4 — tail chaining
removes the `call`/`ret` pair, not the `JitCtx` round-trip, so it is a
different (and much smaller) lever than the one ADR 0002 wanted.**

**Upgrade check.** `cranelift-codegen-0.135.1` is present in the local registry
cache. Diffing it against 0.133.1: the `max_per_class_reg_vals` /
`remaining_reg_vals` lines are byte-identical, and
`supports_tail_calls` is still `Tail`-only. **Upgrading Cranelift unblocks
nothing.**

### Q2 — What exactly would the prologue stub have to be?

First, the bad news: **`trampolines.rs` is not the pattern ADR 0002 meant.** All
16 micro-stub trampolines are plain Rust `unsafe extern "C" fn(*mut JitCtx)`
(`trampolines.rs:215-335`) compiled by rustc. They own no physical registers and
cannot be told to; ADR 0002's "the `trampolines.rs` pattern" reading is a
category error. `grep -rn "global_asm\|core::arch::asm\|std::arch::asm"` over
the whole repo returns **0 hits** — there is no assembly in WIE today, so any
stub is net-new review surface.

What *does* extend is the dispatch half: a host-callable
`extern "C" fn(*mut JitCtx)` stored in the chain table and reached through
`call_indirect` (`trampolines.rs:429-433` mirrors this from Rust). That pattern
transfers directly.

**Stub A — the Tail entry shim (feasible, 2 instructions).** Needed only if tail
chaining is taken. Emitted as `global_asm!` (it must run *before* Cranelift's
prologue, and rustc will not emit a naked function on stable):

```rust
core::arch::global_asm!(
    ".globl wie_block_entry\n\
     wie_block_entry:\n\
     mov  x2, x0          // Tail: arg0 arrives in x2, AppleAarch64: x0\n\
     b    wie_block_body  // tail branch: body's `ret` returns to OUR caller\n",
);
```

- **Shape:** 2 instructions, 8 bytes, no frame, no stack touch.
- **Link:** one absolute `b` to the block's finalized code. Emitted *after*
  `finalize_definitions()`, from `get_finalized_function(func_id)`
  (`lower/mod.rs:1368-1369`).
- **Chain linkage:** this is the load-bearing part of ADR 0002's "link the
  post-prologue body label" idea, and it is **cheap** — a `b` relocates by
  writing a 4-byte little-endian displacement, and `MachBuffer` already emits
  fixups for exactly this (`Inst::IndirectBr` → `targets` relocs,
  `src/isa/aarch64/inst/emit.rs:3064-3068`).
- **Also required:** `trampolines.rs::chain_tail` calls blocks as
  `f(ctx)` with ctx in x0 (`trampolines.rs:429-433`). Under `Tail` it must
  instead call `wie_block_entry`, or the block reads an uninitialised x2.

**Stub B — the x19–x28 residency stub (does not work; see Q3).** Shape *if* it
were viable: save x19–x28 + x14 (11 pairs → 6 `stp`/`ldp`), `ldr` each guest
GPR from `ctx.gpr[i]`, `b` into the body. That is ~28 memory ops on entry and
~28 on exit. For a chain of *N* blocks with live set *L*: today costs `2·L·N`;
the stub costs `56 + 2·L + 2·dirty`. At `L=6, N=4` that is **76 vs 48 memory
ops — the stub is a 1.6× regression**, and only breaks even around `N≈9`. This
amortisation arithmetic is not in ADR 0002 and it is unfavourable at realistic
chain depths.

### Q3 — Register-pressure reality on aarch64

**Arithmetically, 11 residency fits. Expressibility is what fails.**

Cranelift's aarch64 allocator (`create_reg_env`,
`src/isa/aarch64/abi.rs:1643-1734`) has:

| Class | Set | Count |
| --- | --- | --- |
| Int, preferred | `x0`–`x15` | 16 |
| Int, non-preferred | `x19`–`x28` | 10 |
| reserved | `x16` spilltmp, `x17` tmp2, `x18` platform, `x29` FP, `x30` LR, `x31` SP/ZR | 6 |

So Cranelift has 26 allocatable integer registers today. Pinning guest GPRs to
`x19–x28` + `x14` leaves `x0`–`x13`, `x15` = **15 allocatable, zero
callee-saved**. For a 10–12 instruction basic block (the common shape; see F1's
disassembly) 15 registers is ample. **Register pressure is not the binding
constraint** — ADR 0002 was right to worry but wrong to leave it implicit.

Two real costs, though:

1. Removing all callee-saved registers means every spill that previously landed
   in `x19`–`x28` (no prologue cost) must go to a stack slot
   (`get_number_of_spillslots_for_value` → `1` per I64 value,
   `abi.rs:1165-1173`) — a real frame, a real store/reload pair. Low-pressure
   blocks never hit this; SSE-heavy and string-heavy blocks might.
2. **x14 is in the *preferred* set.** It is one of the 16 registers Cranelift
   actually reaches for. Pinning the rflags carrier there costs a hot register
   to buy a cold one.

**The blocker is that residency is not expressible, and forcing it corrupts
state silently.** A stub that parks guest GPRs in `x19`–`x28` before branching
into Cranelift-generated code works against an allocator that (a) still lists
`x19`–`x28` as non-preferred allocatable and (b) emits a prologue that saves
them:

- `is_reg_saved_in_prologue` returns true for `x19`–`x28`
  (`abi.rs:1385-1412`), so Cranelift's prologue emits `stp x19, x20, [sp, #…]` —
  **writing over the guest values the stub just installed**.
- When a block's RA does spill, it picks `x19`–`x28` first among the
  non-preferred set, again over the guest values.

There is no compiler diagnostic for either. The result is a guest whose `rbx`,
`r12`–`r15` silently become whatever Cranelift spilled. `WIE_JIT_VERIFIER=1`
cannot catch it — `docs/RUNBOOK.md:186` states the verifier "never checked x86
*semantics*". **This is a silent-wrong-answer failure mode introduced by a
mechanism whose entire purpose is a performance optimisation, and it is the
reason this ADR's verdict on the full ABI is Blocked rather than "hard".**

**What would unblock it,** in order of realism:

1. Cranelift grows a per-function register-set restriction (a `MachineEnv`
   derived from the signature rather than from `flags`). Nothing in 0.133 or
   0.135 gestures at this.
2. Multiple pinned registers (today: exactly one, x21). Would give at most 1–2
   resident GPRs — enough for a "hottest register stays hot" experiment, not for
   the ABI.
3. Replace Cranelift on the chain path with a hand-written aarch64 backend for
   the hot block shapes. That is a different project with a different cost, not
   a stage of this one.

### Q4 — Is there a partial win that does not need the full ABI?

Assessed on merit. **One is worth taking, and it is much smaller than the
15 ms headline.**

#### ✅ P1 — Narrow the chain-edge store set to the dirty set (recommended)

`writeback_gprs` (`lower/emit.rs:121-131`) stores register *i* when
`dirty[i] || loaded[i]`:

```rust
let do_store = match gpr_dirty {
    Some(d) => d[i] || gpr_loaded[i],
    None    => gpr_loaded[i],
};
```

`gpr_dirty[i]` is a **complete mutation set for `gpr.rs`** — every direct
`gpr[...] = …` there is paired with `mark_dirty` (`gpr.rs:175, 186, 637, 643,
645, 662, 670, 720, 737`), as is every site in `emit.rs` (`:605-697`) and
`mod.rs:1425, 1432, 1550`. The `|| gpr_loaded[i]` disjunct is therefore pure
pessimisation for the store side: it re-stores every **read-only live-in**. In
`-O2` mingw output those are `rbx`, `r12`–`r15` — callee-saved locals that most
blocks only read. It also re-stores the argument registers the fast-`call` path
pre-loads purely to marshal `rcx`/`rdx`/`r8`/`r9`/`rax`
(`emit.rs:888-902`), and it re-stores **dead definitions** — registers the block
writes but never reads, which `analyze_live_gprs` still admits to the live set
because it is a may-touch union (`lower/analysis.rs:322-371`).

**Audit result — two gaps, both in the REP string path, and `string.rs`
references `dirty` zero times** (`grep -n dirty string.rs` → no output):

| Site | What it does | Needed |
| --- | --- | --- |
| `string.rs:223-231` | host `wie_jit_string` mutates `ctx.gpr[0,1,6,7]`; SSA is refreshed from `JitCtx` and `gpr_loaded[0,1,6,7] = true` — **no `mark_dirty`** | `mark_dirty(dirty, 0/1/6/7)` |
| `string.rs:459-466` | same reload after the inline-unrolled helper | `mark_dirty(dirty, 0/1/6/7)` |

Those two `mark_dirty` calls (four lines) plus a comment recording why the REP
path bypasses `write_gpr` are the entire behavioural prerequisite. The existing
doc comment at `emit.rs:98-107` already names this gap; this pass confirms it
is the *only* one.

**Why this is the right scope:** it changes no ABI, no calling convention, no
register ownership, no frame layout. It removes stores that are provably
redundant. Diff size: one predicate plus four `mark_dirty` lines.

**Ceiling.** Per boundary the emitted memory µops are roughly
`2·|live|` (stores + successor loads) `+ 13` (rip store, rflags load/store,
`chain_depth` load + 2 stores, and the 8-load / 4-slot edge-IC probe at
`emit.rs:342-356`). At `|live| = 6`, that is ~25. Dropping read-only live-ins and
dead defs typically removes 2–4 stores, i.e. **~10–15 % of boundary cost**; on
call-heavy guests the fast-call pre-loads add up to 5 more, i.e. **~25 %**. As a
share of the 15 ms figure that is **~1.5–4 ms** — and note the 15 ms figure
itself is unmeasured (F2).

#### ⚠️ P2 — Narrow the entry *load* set (real, but higher risk; not step 1)

The symmetric half. `mod.rs:1034-1041` eagerly loads the whole `live_eff` union
at entry, which **defeats the lazy loader that already exists**:
`ensure_gprs_loaded` (`analysis.rs:390-408`) loads on demand but finds everything
already loaded. Removing the eager loop would shrink the load set to
registers actually read, killing one load + one store per dead-def register.

The hazard is that `read_gpr` **does not check `loaded`** — it returns `gpr[i]`
unconditionally (`gpr.rs:97-100`), and an unloaded slot holds `iconst 0`. A
missed lazy-load therefore yields a **silent zero**, and the audit surface is
every `read_gpr` / `read_gpr_logical` / `read_op_mem` / `effective_addr` caller,
not two sites. Correct as a follow-up, wrong as step 1.

#### ❌ P3 — Specialise chain edges so the successor knows what changed

The waste is entirely on the store side, which P1 already removes. Making the
successor's *load* set depend on the predecessor's dirty mask needs per-edge
recompilation (code-cache blowup, and it collides head-on with the concurrent
relocatable-code-cache work in `jit/`). **Rejected: no headroom over P1, real
risk, YAGNI.**

#### ❌ P4 — Keep the hottest N guest registers resident

Requires binding a CLIF value to a fixed physical register. Blocked by Q1(d) /
Q3, and corrupts silently if forced.

#### ❌ P5 — Attribute part of the 15 ms to the rflags carrier

No. `pass_flags = needs_flags || self_loop` (`mod.rs:994`) and
`needs_flags` is already a precise predicate (`analysis.rs:185-265`), so rflags
is **one 8-byte load and one 8-byte store per block that needs it** — at most
1/17 of the round-trip traffic, ~6 %, and already conditional. Nothing to win.

#### ⚠️ Tail chaining, reframed

Q1(e) shows tail chaining is buildable with a 2-instruction stub. But it removes
the `call`/`ret` pair and the `x30` spill around the call — roughly 4 µops and
6–10 cycles of a ~60-cycle boundary. It **does not remove the `JitCtx`
round-trip at all**; the predecessor still writes back and the successor still
reloads. It is a genuine but ~10 % lever on a cost that is itself 1.5–6 % of
`emu_ms`. Worth recording as feasible; not worth building before P1 and the
counter.

### Q5 — Measurement discipline

**The gate cannot see this lever.** Per F1, `long_loop` is a single self-loop
with zero transitions. `scripts/check.sh --only perf` (budget
`LONG_LOOP_MAX=0.55`, `RUNS=5`, median, `MAX_LOAD=2.0`) would report a null
result for a change that doubled boundary throughput, and would equally fail to
report a 20 % boundary regression. ADR 0002's own target — "`long_loop`
0.25 s → ≤0.10 s" — is unreachable by construction and should be struck.

The discipline, in three layers:

**Layer 0 (prerequisite, deterministic, noise-free): add the counter.** Until a
chain-hop counter exists, no timing claim about this lever is falsifiable. Two
`JitCtx` fields and one emitted `add` each:

- `chain_hops: u64` — bumped where `chain_depth` is incremented
  (`emit.rs:327-328`) and in `trampolines.rs::chain_tail`.
- `chain_store_ops: u64` — the store set is compile-time-known, so emit
  `ctx.chain_store_ops += POPCOUNT(store_set)` with a literal. One add per
  boundary, no runtime popcount.

Then the entire win is a **counter delta with zero host noise**: P1 reduces
`chain_store_ops` while leaving `chain_hops` and `jit_insns` bit-identical.
Report both on the `WIE_RUNTIME_PROFILE=1` line. This converts an unmeasurable
timing question into a deterministic arithmetic one, and it is the highest-value
item in this ADR.

**Layer 1 (timing): paired, interleaved, single binary.** Adopt the discipline
already used for `WIE_JIT_VERIFIER` (`docs/RUNBOOK.md:186`, `docs/status.md:42`):

- **N = 15 interleaved A/B pairs**, alternating arms within each pair. 15 is the
  house precedent and gives an exact two-sided sign test; at p = 6e-5 the
  observed verifier effect would have been 15/15.
- **Statistic: the median of the 15 per-pair ratios**, plus a **sign test on the
  pair signs** (exact binomial, no normality assumption).
- **Both arms from one binary.** P1 must be an env knob read at lowering time
  that selects the store predicate — e.g. `WIE_JIT_CHAIN_STORES=dirty|loaded` —
  exactly as `WIE_JIT_VERIFIER` does. Cross-build comparison is contaminated by
  rustc/Cranelift code-layout drift that an in-binary switch is not.
- **Deadband:** pre-register ±5 %. A median ratio inside it is "no change", full
  stop — do not report it as a small win. Rationale: the host's own idle
  long_loop spread is 0.26 s median against 0.5–0.7 s at load 6–8, i.e. up to
  2.7× contamination, so a ±5 % deadband on a *paired* design is the floor, not
  a stretch.

**Layer 2 (workload): long_loop is disqualified; add a chain-heavy fixture.**
The validation workload must actually produce boundaries. Recommend a new
`micro-exes/` fixture built from two or three mutually-recursive small functions
with no inner loops — guaranteed many small non-loop blocks, hence real chain
hops, while staying deterministic and output-assertable. `7za.exe` (needs
`./scripts/fetch.sh 7za`; gitignored) stays as the real-guest confirmation arm
but cannot be the gate.

**Correctness gate, separate from timing.** `cargo nextest` plus the
JIT-vs-iced differential on a chain-heavy workload. P1's failure mode is a
stale register, which the differential catches and timing never will. Note the
suite must run in the arm that exercises the REP path (`string.rs`) — the
`cpu_string` / `heap_alloc` fixtures.

### Q6 — Failure modes

Ordered by how bad they are, not by likelihood.

| # | Failure | Symptom | Detectable by |
| --- | --- | --- | --- |
| 1 | Stub installs guest GPRs in `x19`–`x28`; Cranelift's prologue `stp`s over them (`abi.rs:1385-1412`) or RA picks them as non-preferred spill targets | **Silently wrong guest results.** `rbx`/`r12`–`r15` become spill values. Guests misbehave far from the JIT; 7-Zip-style tools recurse or write through null bases | JIT-vs-iced differential only. `WIE_JIT_VERIFIER=1` cannot see it (`RUNBOOK.md:186`) |
| 2 | Store predicate narrowed to `dirty` while a `mark_dirty` gap remains | **Silently stale registers** in the successor. Same class as a missed `mov` in any compiler | JIT-vs-iced differential; the two `string.rs` sites in Q4/P1 are the only known gaps |
| 3 | Entry load set narrowed without full lazy-load coverage | **Silent zero** in a guest register — `read_gpr` returns the `iconst 0` initialiser for an unloaded slot (`gpr.rs:97-100`) | Differential; also tends to crash loudly, which is the good case |
| 4 | Stub fails to restore `x19`–`x28` before `ret` | **Host frame corruption.** Rust caller's callee-saved values are clobbered — a `&mut Vec` or index in the dispatcher becomes garbage → delayed abort or silent heap corruption, attributed to unrelated code | Usually a crash, but possibly minutes later. Worst outcome for triage |
| 5 | Stub's `b` displacement written past the end of a finalized `MachBuffer` | Host memory corruption at an arbitrary address | Crash, usually immediate |

**Worst outcome: #1 or #4.** #1 produces a JIT that computes wrong answers with
full reporting — the failure mode this project has been bitten by before (the
`WIN32_FIND_DATA` offset that sent 7-Zip into infinite recursion, per
`CLAUDE.md`). #4 corrupts the *host*, not the guest, and is the hardest to
attribute back to the JIT.

**Mitigation that follows from the table:** because the full ABI's failure modes
are silent or host-corrupting, and its ceiling is ~1–3 % of `emu_ms` (F3), the
asymmetry is decisive. P1's failure mode is #2 — the same class as any missed
lowering case, covered by the differential the project already runs, with a
two-site audit. **A change whose worst case is a caught differential beats a
change whose worst case is a silently wrong guest.**

## Decision

**The direct-register ABI is Blocked.** Not by the result-register window (real
but bypassable, `Q1(a)`), and not by the absence of a tail-call convention
(real but two instructions away, `Q1(e)`) — but by the absence of **any**
mechanism to bind a CLIF value to a fixed physical register or to remove
`x19`–`x28` from the allocator for one function: `AbiParam` has no register
field (`ir/extfunc.rs:139-146`), `MachineEnv` is a process-static chosen only by
`enable_pinned_reg` (`machinst/abi.rs:1542-1544` → `isa/aarch64/abi.rs:1179-1188`),
the pinned register is exactly one (x21, `inst/regs.rs:19`), and no register
hints reach regalloc2 0.15.1. Forcing it with a stub corrupts guest registers
silently. Upgrading to 0.135.1 unblocks nothing (verified by diff).

**Proceed with P1 instead**, scoped to two steps:

**Step 0 — instrumentation.** Add `chain_hops` and `chain_store_ops` to
`JitCtx`, bump them at `emit.rs:327` / `trampolines.rs::chain_tail` /
`writeback_gprs`, and surface both on the `WIE_RUNTIME_PROFILE=1` line. Report
`chain_hops`, `chain_hops / jit_insns`, and `chain_store_ops` for `long_loop`
(expected: ~0 hops — the fact that makes F1 visible in CI rather than in prose),
the new chain-heavy fixture, and `7za.exe`. **No behaviour change; this is a
read-only addition and it is what makes step 1 falsifiable.**

**Step 1 — narrow the chain-edge store set to `dirty`.** Two parts, both small:

1. `lower/emit.rs:121-131` — store iff `dirty[i]`, not `dirty[i] || loaded[i]`.
   Keep the `None` arm (micro-stub paths that pass no dirty mask) unchanged.
2. `lower/string.rs:223-231` and `:459-466` — add `mark_dirty` for
   `{0, 1, 6, 7}` in both REP paths, with a comment recording that the host
   string helper mutates `ctx.gpr` directly and therefore bypasses `write_gpr`.
3. Gate behind `WIE_JIT_CHAIN_STORES=dirty|loaded` (default `loaded`, i.e.
   **opt-in**), read in `JitConfig`, so one binary serves both A/B arms.

Not in scope for this ADR, in order: P2 (narrow the entry load set — real, but
its audit surface is every read site and its failure is a silent zero), tail
chaining (feasible per `Q1(e)`, ~10 % of a cost that is 1.5–6 % of `emu_ms`),
and the x19–x28 residency design (blocked; revisit only if Cranelift grows a
per-function register-set restriction).

## Alternatives considered

- **A. Build the full direct-register ABI with a hand-written stub** (ADR 0002
  step 2). Rejected: not expressible in Cranelift 0.133/0.135 (Q1(d)); silently
  corrupting if forced (Q3); and the amortisation is *negative* at realistic
  chain depths — ~76 vs ~48 memory ops for a 4-block chain at `|live| = 6`
  (Q2, Stub B). Rejected on three independent grounds, any one of which suffices.
- **B. Tail chaining first** (ADR 0002 step 1). **Feasible, deferred.** The
  prior "blocked" verdict was wrong: `ReturnCallInd` emits a bare `br`
  (`inst/emit.rs:3061-3075`) and the entry mismatch is `mov x2, x0; b body`. But
  it removes `call`/`ret`, not the `JitCtx` round-trip, so it is ~10 % of a cost
  that is 1.5–6 % of `emu_ms`. Cheapest of the ABI-adjacent options; wrong
  first move.
- **C. Keep the `JitCtx` round-trip unchanged** (ADR 0002's option A). Zero risk,
  and it is the status quo — but it forgoes ~1.5–4 ms of provably-redundant
  stores for a four-line audit. Rejected as "no risk" is not a reason to leave
  dead stores on the hot path when the audit is this small.
- **D. Specialise chain edges per predecessor** (Q4/P3). No headroom over P1,
  code-cache blowup, collides with the in-flight relocatable-cache work. Rejected
  on YAGNI and collision risk.
- **E. Widen the JIT TLB / add trace-level superblocks to reduce boundary count
  instead.** ADR 0002 kept this as an orthogonal follow-up (`WIE_JIT_TRACE`).
  Still orthogonal, still unbuilt, and it reduces *boundaries* rather than
  boundary *cost* — but it composes with P1 rather than competing with it.

## Consequences

- ADR 0002's `Decision` and its 2026-09-05 feasibility section remain the dated
  historical record. **This ADR supersedes its three load-bearing claims**:
  (i) tail chaining is blocked — it is not, it is two instructions away;
  (ii) the 17-result window is the constraint — it is real but bypassable and
  not load-bearing; (iii) the register window is the constraint — the actual
  constraint is the *absence of a fixed-register binding*, which ADR 0002 did
  not identify. ADR 0002's expected outcome, "`long_loop` 0.25 s → ≤0.10 s", is
  **withdrawn as unreachable** (F1).
- ADR 0002's reversibility claim ("opt-out via `WIE_JIT_DIRECT_REGS=0`") was
  already void as of 2026-09-25 and stays void. P1 introduces a real bisect
  switch, `WIE_JIT_CHAIN_STORES`, defaulting to today's behaviour.
- `WIE_JIT_DIRECT_REGS` and `WIE_JIT_TAILCHAIN` stay deleted. `RUNBOOK.md:155-156`
  is accurate as written; no change needed. `crates/wie-runtime/tests/runbook_knobs.rs:55-56, 310`
  asserts these two are *absent* from the knob table — that test stays green and
  is the correct guard against re-advertising a dead knob.
- `docs/implementation-plan.md:46` should drop the inference that "`WIE_JIT_CHAIN=0`
  is not slower than chain-on (0.238 s vs 0.263 s on `long_loop`) … so chaining
  is not where the remaining time is". That comparison is void: `long_loop` has
  zero chain hops (F1), so it measures nothing about chaining, and
  `jit_chain avg_width` measures table population, not hit rate (F2). The row's
  conclusion may well be right; its evidence is not.
- Adding a chain-heavy `micro-exes/` fixture is required for step 1's validation
  and is independently useful: it is the first deterministic, gitignored-free,
  output-assertable workload that exercises the chain path at all.
- Step 1 reduces stores on the chain path only. Dispatcher-bounce cost is
  untouched — `run_compiled`'s exit does a full 16-GPR + 16-XMM copy into the
  engine (`pipeline.rs:1328-1351`), which is a separate and larger item,
  deliberately not in scope here.

## Validation

**Step 0 (instrumentation).** `WIE_RUNTIME_PROFILE=1` gains `chain_hops` and
`chain_store_ops`. Acceptance: `long_loop` reports `chain_hops ≈ 0` (making F1 a
CI-visible fact); the new chain-heavy fixture reports a non-trivial
`chain_hops / jit_insns`; `7za.exe i --max-api 400000` reports the real-workload
boundary rate that replaces the unsourced "750 k per 100 M instructions". **Also
produces the ceiling arithmetic directly** — `ceiling_ms ≈ chain_hops ×
µops_saved / clock_Hz` — before any code changes. If that arithmetic comes out
under ~1 ms on the target workload, stop here; steps below are not warranted.

**Step 1 (store narrowing).**

- `cargo nextest run --workspace` — and specifically the REP-string fixtures
  (`cpu_string`, `heap_alloc`) that exercise the two `mark_dirty` additions.
- JIT-vs-iced differential on the chain-heavy fixture: identical output.
- `scripts/run-micro-suite.sh` green in both arms
  (`WIE_JIT_CHAIN_STORES=dirty` and `=loaded`).
- **Deterministic assertion (the real acceptance test):** with
  `=loaded` vs `=dirty` on the same binary and same guest, `jit_insns`,
  `chain_hops` and guest output are **bit-identical**, and `chain_store_ops` is
  strictly lower. This needs no timing and cannot be faked by host noise.
- Timing per Q5 Layer 1: N = 15 interleaved pairs on the chain-heavy fixture,
  median of per-pair ratios, sign test, ±5 % deadband. Expect a null on
  `long_loop` — **a null there is the predicted result, not a failure**, and
  should be recorded as such.
- `WIE_JIT_VERIFIER=1` on a debug build while iterating (it will not catch this
  class of bug, per `RUNBOOK.md:186`, but it catches ill-typed lowering).

**Guard against a guard never seen failing.** Before landing step 1, confirm the
new `mark_dirty` sites are load-bearing by temporarily removing one and
verifying the differential fails. A narrowing that has never been seen to break
has not been shown to be safe.

## Reversibility

**Step 0** is purely additive counters — delete the two `JitCtx` fields and their
`WIE_RUNTIME_PROFILE` line; no production behaviour depends on them.

**Step 1** is default-**off**: `WIE_JIT_CHAIN_STORES` defaults to `loaded`, which
is today's emitted predicate. `=dirty` is opt-in. One env var restores the
existing code path exactly, in the same binary — the `WIE_JIT_VERIFIER` shape,
and the reason it is preferred over a build-time switch is that a single binary
keeps both A/B arms free of cross-build codegen drift.

Stageable: land step 0 standalone; land the two `mark_dirty` sites standalone
(they are pure additions, no behaviour change — `writeback_gprs` still stores on
`loaded`); then flip the predicate under the knob. If the sign test comes back
null on the chain-heavy fixture, the knob stays off and the counter from step 0
stays — it is worth more than the change.

## What this pass could not determine

- **The real boundary rate on a real workload.** No chain-hop counter exists
  (F2), and the host was at load average 10.8 throughout this pass, above
  `check.sh`'s own `WIE_PERF_MAX_LOAD=2.0` refusal threshold, so no timing here
  would have been trustworthy. Step 0 settles it in one run.
- **Whether P1 actually pays on 7-Zip.** The 10–25 %-of-boundary estimate is
  derived from µop counting and a read of the emitted IR, not measured. It
  depends on the live-set/dirty-set ratio in mingw `-O2` output, which is
  workload-specific.
- **Whether `CallConv::Tail` blocks are byte-compatible with Rust entry in
  practice.** The x0/x2 argument difference is verified from
  `abi.rs:178-190`, and the `ret`-to-`x30` behaviour follows from `ReturnCallInd`
  emitting no trailing `ret`. Not verified: interaction with `unwind_info`
  (default on, but `abi.rs:644` gates unwind emission on `AppleAarch64` only, so
  `Tail` blocks would carry none) and with `MachBuffer`'s island/reloc handling
  when the `b` target is out of range. A 20-line experiment settles both.
- **The chain-depth distribution in real guests.** `MAX_CHAIN_DEPTH` is 48
  (`lower/mod.rs:482`), and the average number of blocks per chain determines
  whether ADR 0002's own amortisation argument (Stub B) could ever have paid.
  Step 0's `chain_hops` versus `run_compiled` entry count answers it.