# ADR 0002 — Direct block-to-block register hand-off + SSA rflags (Q8/C)

Status: Accepted · 2026-08-26 — grill-with-docs Q8

## Context

After Q3/B (pooled JIT workers) the throughput limiter measured in `jit-architecture-review.md:42` is the per-block `JitCtx` round-trip: every compiled block loads all live GPRs on entry and stores dirty ones on exit even when chained to a successor that only changes 1–2 regs. 750 k transitions @ 364 M insns cost ~15 ms load/store traffic, stealing memory bandwidth from the pooled surface blits (Q2/D) and capping `long_loop` at ~16 M insn/s vs the Q1/D target 50 M.

## Decision

Fixed native ABI for chained execution:

1. Guest GPRs stay in native callee-saved regs across chains (`x19–x28` = `rax…r15` fixed order, `x14` = rflags carrier).
2. `block → chain_lookup → successor` chains via `b`/`br` that keep live regs in place; only dispatcher fallback (miss, SMC invalidate, `fake-VA` host stop) spills/restores via `JitCtx`.
3. `rflags` become SSA `i1` booleans per ALU (`PendingFlags` → `zf, sf, cf, of` SSA), so `test; je` consumes ZF directly without packed-bit pack/unpack.

## Alternatives considered

- **Keep JitCtx round-trip (A):** no risk, but cannot reach Q1/D (50 M insn/s) while presenting.
- **Larger traces only (B):** fewer boundaries but still copies at each boundary; orthogonal — we keep it as follow-up (`WIE_JIT_TRACE`) behind the same flag.

## Consequences

- New codegen prologue/epilogue in `wie-cpu/src/jit/lower/*` + `engine.rs` trampoline.
- Shadow stack/chain table unchanged; only spill sites change.
- Expected: `long_loop` 0.25 s → ≤0.10 s, ~10 ns/boundary saved + store-forward stalls removed.

## Validation

`cargo nextest` + `criterion` `long_loop` vs `WIE_CPU=iced` oracle, `run_until_stop` per-block transition counters, `gen_bumps` stability. **Wave3 default on** — harness is `WIE_RUNTIME_PROFILE=1` 40 s capture, `wake→present` P95 <16.6 ms, `long_loop` pooled. Opt-out via `WIE_JIT_DIRECT_REGS=0` for matrix (was `WIE_JIT_DIRECT_REGS=1` gated).

## Reversibility

Default **on**; opt-out via `WIE_JIT_DIRECT_REGS=0`/`false`/`off` restores existing `JitCtx` spill path. Wave3 flipped the gate (was opt-in).


## Implementation status (2026-09-02 audit)

**NOT implemented.** The "default on" claim above was drift: `WIE_JIT_DIRECT_REGS`
is parsed (`direct_regs_enabled()`) but has **zero call sites**; no x19–x28
guest-register mapping exists in `jit/lower/*`, and every block boundary and
chain hop still pays the full `JitCtx` GPR/XMM round trip. This remains the
top JIT throughput lever (Wave 3 of the architecture-review roadmap); the
feasibility assessment (trampoline/prologue shape, SSA rflags lowering order,
oracle diffing vs `WIE_CPU=iced`) is tracked in
`docs/implementation-plan.md`.
