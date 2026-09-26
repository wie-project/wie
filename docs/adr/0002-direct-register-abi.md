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


## Implementation status (2026-09-05 feasibility pass)

**Feasibility pass complete; staged implementation begun.** Findings:

- **Spill-site enumeration** (the per-edge cost this ADR removes):
  dispatcher entry mass-copy (`pipeline.rs run_compiled`), block-entry
  live-GPR/rflags loads (`lower/mod.rs compile_block`), the per-chain-edge
  `writeback_gprs` + successor reload (`lower/emit.rs emit_chain_or_exit` —
  executed on every block edge, the 750k-transitions tax), block-exit
  stores, and dispatcher exit writeback. Full list in
  `docs/implementation-plan.md` Wave 3 row 1.
- **Tail-call chaining (REVERTED to default-off — never landed)**: commit
  f415d6b switched chain hops to `return_call`/`return_call_indirect`
  (`WIE_JIT_TAILCHAIN`, default on) — the successor reuses the caller frame
  (no prologue/epilogue/ret per hop), with the `MAX_CHAIN_DEPTH` counter
  deliberately KEPT as the periodic dispatcher bounce. **Every block with a
  chain edge failed Cranelift verification on every platform**: only
  `CallConv::Tail` supports `return_call` in Cranelift 0.133, and the block
  signature must stay on the host default (`AppleAarch64`/`SystemV`) to
  remain callable from Rust as `extern "C"` — the verifier rejects it with
  "calling convention `…` does not support tail calls". The gate was flipped
  back to default-off (`WIE_JIT_TAILCHAIN=1` force-enables for experiments)
  and the shipped hop is the nested-call + `MAX_CHAIN_DEPTH` guard shape.
  Tail chaining stays viable only once the prologue stub (below) lets blocks
  adopt a tail-call-capable convention or Cranelift lifts the restriction.
- **ABI constraint (the load-bearing finding)**: literal x19–x28 residency
  cannot be expressed through Cranelift under the standard aarch64 C ABI —
  a 17-result block signature does not fit the result register window, and
  no custom call-convention hook is available. The viable staged path is:
  (1) tail-call chaining (blocked — see above; Cranelift rejects
  `return_call` under every ABI convention), (2) a hand-written per-block
  prologue stub (the `trampolines.rs` pattern) that shuffles x19–x28/x14
  against the C-ABI frame with chaining linking the post-prologue body
  label, (3) SSA rflags layered onto that stub boundary.
  `WIE_JIT_DIRECT_REGS` remains reserved for step 2; the SSA-rflags design
  (pack/unpack deletion, `PendingFlags` → i1 booleans, `flag_cond` consuming
  ZF/SF/CF/OF directly) is unchanged and ordered after the stub.

## Status note (2026-09-25) — inert surfaces removed

The **Decision** and **2026-09-05 feasibility** sections above are the dated
historical record and are unchanged, but two of the knobs they name are no
longer live:

- **`WIE_JIT_DIRECT_REGS` is gone from the code.** `JitConfig::direct_regs_enabled`
  and its `WIE_JIT_DIRECT_REGS` parse were deleted 2026-09-25 from
  `crates/wie-cpu/src/jit/config.rs`. It had **zero lowering call sites** (no
  x19–x28 handoff exists), so the "Default **on** … opt-out" reversibility claim
  in this ADR never described a real second code path. The direct-register ABI
  remains an **open** item: the plan is a per-block prologue stub (step 2)
  followed by SSA rflags at that boundary (step 3). When it lands it will need
  a genuine bisect switch, which this ADR must then re-advertise.
- **`WIE_JIT_TAILCHAIN` is gone too.** The tail-call emission
  (`return_call` / `return_call_indirect`) and its config gate were deleted
  2026-09-25 from `crates/wie-cpu/src/jit/lower/emit.rs` — the branch is dead
  code by construction, since Cranelift 0.133's verifier rejects it under every
  calling convention the block signature can use. The **shipped** hop is the
  nested host C ABI `call`/`call_indirect` with the `MAX_CHAIN_DEPTH` guard,
  the monomorphic edge IC, the late-bound chain table, and the dispatcher
  fallback, all unchanged. Tail chaining stays viable only once the prologue
  stub lets blocks adopt a tail-call-capable convention or Cranelift lifts the
  restriction; until then it is a blocked design, not a debug knob.
