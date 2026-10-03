# ADR 0007 — One operation-class table for JIT admission and lowering

Status: Proposed · 2026-10-01 — maintainability plan T3.1

## Context

`wie-cpu` holds two x86-64 semantic implementations: a Cranelift block JIT and
an iced-x86 interpreter fallback. **Integer** semantics are duplicated across
three mnemonic tables. Distinct `Mnemonic::` identifiers per file, counted
uniformly so the three are comparable:

| Table | Location | Distinct mnemonics |
| --- | --- | --- |
| interpreter dispatch | `exec/mod.rs:255-757` | 337 |
| JIT admission gate | `jit/block.rs:227-548` | 270 |
| JIT lowering dispatch | `jit/lower/insn.rs:277-838` | 230 |

The tables are near-identical in shape and ordering, and operand *forms* are
re-decided independently in each: `mov_is_lowerable` (`block.rs:952`) vs
`exec/mod.rs:296` vs `lower_mov` (`insn.rs:307`). The same triple-decision
repeats for `xadd`/`cmpxchg`/`bt*` (`block.rs:274-311`), shifts (`:769`), cmov
(`:791`), setcc (`:803`), and div (`:834`).

Condition codes are written twice — `exec/ops.rs:45 cond_from` and
`jit/lower/mod.rs:1446 cond_from_bits`, the latter enumerating `Jcc | Cmovcc |
Setcc` triples in a single match, so **adding one condition means editing a
three-way triplicated list**. Three parallel op enums exist for the same
concepts (`exec/ops.rs:10/18/30`, `jit/lower/gpr.rs:55`, `jit/lower/insn.rs:37`),
plus a fourth in `exec/x87.rs:131`.

**Measured facts that bound the work.**

- The union of mnemonics named anywhere in `jit/lower/` is 248, and **all 248
  exist in `exec/`** (set difference: zero). The JIT never handles something the
  interpreter cannot.
- `block.rs \ insn.rs` = 41 mnemonics — terminators routed to `lower_term` /
  `try_lower_inline_rep`, plus `Enter`/`Pushf`/`Popf`, which have admission arms
  but no lowering arm and can therefore only be rejected at lowering time.
- `insn.rs \ block.rs` = **exactly one**: `Cqo` (`lower_insn:316`). A dead arm.
- `exec` handles **89** mnemonics the JIT does not.

**The cost.** A bug fix in integer semantics needs three edits plus a matching
`analysis::mark_insn_gprs` entry, and **missing the fourth silently emits
`iconst 0`**. The crate documents this exact hazard at `block.rs:279-292`, and
`jit/tests/implicit_operands.rs` exists only to catch it afterwards.

The SIMD half of the crate already solved this. `exec/sse_types.rs:27-264` plus
`exec/sse.rs:1131-1525` define each SSE/FP op once with an explicit ABI
encoding, and *both* engines consume it — 36 interpreter call sites, 7 JIT sites.
`StringOpKind` is shared the same way. So the pattern is proven here, not
hypothesised.

### Interaction with in-flight work

`jit/block.rs` — the admission gate this decision restructures — is being
modified concurrently for the implicitly-locked-ALU work. This ADR records the
decision; the implementation must be sequenced **after** that change lands, or
the two will conflict. The decision itself is unaffected.

## Decision

Introduce one `OpClass` table consumed by **both** `is_lowerable` (in
`jit/block.rs`) and `lower_insn`'s pre-check (in `jit/lower/insn.rs`):

```rust
enum OpClass {
    Gpr { forms: FormSet },
    Sse { forms: FormSet },
    String,
    Term,
    Unsupported,
}
```

Leave `exec` alone.

**Why these two tables and not all three.** `is_lowerable` and `lower_insn` are
the pair that must agree **by construction**: if the gate admits an instruction
the lowering cannot handle, the failure is the silent `iconst 0` above. The
interpreter is not on that path — it dispatches on the decoded instruction
directly and always has a fallback. Unifying all three would couple two
engines that are already deliberately divergent in 89 mnemonics, and would buy
nothing for the actual hazard.

## Alternatives considered

- **A. One `OpClass` table for admission + lowering** (recommended). DRY on the
  two tables that must agree by construction; eliminates the silent-`iconst 0`
  class. Cost: one lookup on the **compile** path — irrelevant, since a block is
  compiled at most once per VA. No semantic change, mechanically reversible.
- **B. Extend the SSE pattern to flags** — make `exec` own
  `flags_add/sub/logic/zs_pf` as pure `(u64 result, u64 a, u64 b, width) ->
  u64 rflags`, with the JIT calling them as Cranelift libcalls. True single
  source of truth, but a **host call per flag-setting instruction on the hottest
  path**. The existing `lower_arith_lazy` deferred-flag machinery exists
  precisely to avoid that. **Rejected** — it would regress the `long_loop` pin
  that CONTRIBUTING rule 2 makes a pass/fail condition.
- **C. Leave the duplication; add a generated differential test** over one
  `(encoding, expected)` list asserted against both engines. KISS, zero perf
  risk, but it *detects* drift rather than removing the maintenance cost.
  **Adopted as the companion to A, not as a replacement** — belt and braces, and
  it covers the flag semantics that A deliberately does not unify.

## Consequences

- Adding an x86 mnemonic becomes: one `OpClass` row, plus the lowering body,
  plus the interpreter arm. The failure mode where the gate and the lowering
  disagree becomes a compile error instead of a silent `iconst 0`.
- The `Cqo` dead arm is removed as a side effect — the table makes the
  inconsistency visible.
- `block.rs \ insn.rs`'s 41 terminators and `Enter`/`Pushf`/`Popf` are modelled
  as `Term` rather than three special cases, so the "admitted but not lowerable"
  class has one home instead of being implicit.
- **Perf risk must be measured, not assumed.** `long_loop` is pinned at
  0.40–0.55 s release JIT (0.28–0.36 s at `WIE_JIT_OPT=speed`) and is a pass/fail
  condition. Take best-of-N on a quiet host before and after; interference only
  ever adds time, so a single run cannot distinguish regression from noise.
- The 89-mnemonic divergence is *documented* rather than eliminated. That is
  correct: each entry has a specific reason (32-bit `div` needs implicit
  RDX:RAX as i128 at `block.rs:279-292`; implicitly-locked memory RMW needs
  `block.rs:883-908`; 64-bit `mul` is separate; non-`is_near_branch` control
  flow falls back).

## Validation

1. `cargo nextest run -p wie-cpu` — no regression; the JIT differential and
   `implicit_operands` suites are the specific ones that must stay green.
2. `./scripts/run-micro-suite.sh long_loop` — compare against the pinned budget,
   best-of-N, before and after.
3. `./scripts/run-micro-suite.sh --matrix` — exercises `WIE_CPU=iced` and
   `WIE_JIT_MEM=slow/pin`, which is where an admission/lowering divergence would
   surface as an engine-dependent behaviour difference.
4. New differential test per alternative C, asserting both engines agree per
   encoding. This is what keeps flag semantics honest when they are deliberately
   *not* unified.

## Reversibility

High. A is a refactor with no semantic change: the table can be inlined back,
and no gate or behavioural switch is involved. Should it regress `long_loop`,
reverting the table restores the previous compile path exactly.

## Not decided here

Whether `GuestMemBackend` (`mem/backend.rs:28`) should get a second
implementation or be deleted — it has exactly one impl and exists for tests. And
whether `FastTlb` (`mem/mod.rs:83-156`) should merge with `GenTlb`: it is
direct-mapped and single-bounds-checked by design, and the set-associative
version is measurably slower on the interpreter path, so this ADR recommends
**against** it.