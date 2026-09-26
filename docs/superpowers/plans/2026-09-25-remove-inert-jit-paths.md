# Remove Inert JIT Paths Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Remove the parsed-but-unused direct-register flag and the experimentally unusable tail-call emission, then make current documentation stop advertising either as a live gate.

**Architecture:** Delete `direct_regs_enabled` from `JitConfig` because it has zero lowering call sites. Delete `tail_chain_enabled` and its `return_call` emission branch because Cranelift rejects the required ABI tail calls; retain the shipped nested-call path, depth guard, edge IC, chain table, and dispatcher fallback. Update current status/docs while preserving dated historical design notes.

**Tech Stack:** Rust, Cranelift 0.133, Bash, `cargo nextest`, release `long_loop` benchmark.

**Spec:** `docs/adr/0002-direct-register-abi.md:30-73`, `docs/implementation-plan.md:41-50`, `docs/RUNBOOK.md:84-140`, and commit `bcf3880` (tail-call revert).

## Global Constraints

- Do not remove `WIE_JIT_CHAIN`, `WIE_JIT_SSA_FLAGS`, `WIE_JIT_MEM`, `WIE_JIT_SIMD`, `WIE_JIT_STRING_BULK`, `WIE_CPU=iced`, or any other live differential-testing oracle.
- Do not change the shipped nested-call chain implementation, `MAX_CHAIN_DEPTH`, edge IC, chain table, SMC guard, or dispatcher fallback.
- Do not implement direct-register ABI or a new calling convention in this cleanup.
- Do not change D3D9 capture, headless raster, degrade-not-die, or Wave 5 behavior.
- Do not rewrite dated historical handoff/review prose; add or update only current status notes.
- Do not add dependencies, commits, or branch switches.
- Preserve strict workspace lints and release-only performance measurements.

---

### Task 1: Remove the Inert Direct-Register Flag

**Files:**
- Modify: `crates/wie-cpu/src/jit/config.rs:87-94,285-305,430-447`
- Modify: `docs/RUNBOOK.md:84,101,138`
- Modify: `docs/implementation-plan.md:45-50`
- Modify: `docs/status.md:40,46`
- Modify: `docs/architecture/cpu-and-memory.md:55-62`
- Modify: `docs/adr/0002-direct-register-abi.md:30-35,66-73` (status note only; preserve design rationale)

**Interfaces:**
- Consumes: no runtime behavior; the flag has no readers outside its own config accessor.
- Produces: no `WIE_JIT_DIRECT_REGS` configuration surface.

**Validation owner:** JIT/config owner.

- [x] **Step 1: Record the dead-code proof**

```bash
rg -n 'direct_regs_enabled|WIE_JIT_DIRECT_REGS|direct_regs' crates/wie-cpu/src
```

Expected before removal: only the field, env parse, accessor, and comments; no lowering or execution call site.

- [x] **Step 2: Remove the field and initialization**

Delete this field and its dead-code attribute from `JitConfig`:

```rust
#[allow(dead_code)]
direct_regs_enabled: bool,
```

Delete the `direct_regs_enabled: ...` initializer from `JitConfig::from_env`. Leave all other `JitConfig` fields and parsing unchanged.

- [x] **Step 3: Remove the accessor and stale comments**

Delete `direct_regs_enabled(&self) -> bool`. Remove comments claiming `WIE_JIT_DIRECT_REGS=0` restores an implemented fallback. Do not replace them with a new no-op knob.

- [x] **Step 4: Update current documentation truth**

Remove the RUNBOOK rows for `WIE_JIT_DIRECT_REGS`; keep the direct-register design/status as an open implementation item in `docs/implementation-plan.md` and ADR-0002, but state that the env flag is gone and no ABI exists. Correct `docs/architecture/cpu-and-memory.md` so it does not claim default-on direct-register handoff. Preserve the ADR's feasibility rationale and dated status history.

- [x] **Step 5: Verify the flag is absent**

```bash
rg -n 'WIE_JIT_DIRECT_REGS|direct_regs_enabled|direct_regs' crates
```

Expected: no matches. `docs/adr/0002-direct-register-abi.md` and historical prose may mention the removed flag as history, but current status must say it is not implemented.

---

### Task 2: Remove the Unusable Tail-Call Emission

**Files:**
- Modify: `crates/wie-cpu/src/jit/config.rs:87-91,219-232,445-455`
- Modify: `crates/wie-cpu/src/jit/lower/emit.rs:199-340`
- Modify: `docs/adr/0002-direct-register-abi.md:48-62` (status note only)

**Interfaces:**
- Consumes: the existing nested-call chain path and its `MAX_CHAIN_DEPTH` guard.
- Produces: one chain emitter with no `WIE_JIT_TAILCHAIN` branch and no `return_call*` instructions.

**Validation owner:** JIT/lowerer owner.

- [x] **Step 1: Record the current chain oracle**

```bash
cargo nextest run -p wie-cpu -E 'test(chain) or test(block) or test(jit)'
cargo build -p wie-cli --release
```

Expected before removal: the selected JIT tests pass and the release CLI builds. These are the behavior baseline for the nested-call path.

- [x] **Step 2: Remove the config field and parser**

Delete `tail_chain_enabled: bool` from `JitConfig`, its `WIE_JIT_TAILCHAIN` match in `from_env`, and its accessor. Keep the explanatory ADR note that the experiment was reverted/blocked, but remove the live env knob from the runbook/current status.

- [x] **Step 3: Remove the tail-call branch only**

In `emit_chain_or_exit`, delete the complete conditional beginning at:

```rust
if super::super::config::JitConfig::get().tail_chain_enabled() {
```

and ending after its tail-call `return`. This includes the `return_call`, `return_call_indirect`, tail-specific depth bounce, and the branch-local IC/chain-table duplicate. Leave the existing nested-call path beginning at the `// Host-stack guard: each hop nests a C frame.` comment untouched, including its depth guard, edge IC, late-bound chain table, and `jump_exit` fallback.

- [x] **Step 4: Correct the emitter documentation**

Rewrite the function comment to describe only the shipped nested-call hop:

```rust
/// Writeback + set RIP + chain to the successor (nested host call).
///
/// Blocks remain callable from Rust through the host C ABI. Past
/// `MAX_CHAIN_DEPTH`, the emitter returns to the Rust dispatcher with RIP
/// already advanced. Every edge keeps the invalidation-generation guard.
```

Remove all references to `WIE_JIT_TAILCHAIN`, `return_call`, and an alternate hop shape from current code comments. Preserve the historical explanation in ADR-0002 as a dated status note.

- [x] **Step 5: Verify no tail-call path remains**

```bash
rg -n 'WIE_JIT_TAILCHAIN|tail_chain_enabled|return_call|return_call_indirect' crates/wie-cpu/src
```

Expected: no matches. Run the focused JIT tests and `cargo fmt --all --check`.

---

### Task 3: Validate the Cleanup Without Removing Oracles

**Files:**
- No source files beyond Tasks 1–2.
- Optional current-status documentation already listed in Task 1.

**Interfaces:**
- Consumes: the nested-call JIT and the live config matrix.
- Produces: evidence that only inert paths were removed.

**Validation owner:** Repository and performance owners.

- [x] **Step 1: Run static legacy-path proof**

```bash
matches=$(rg -n 'WIE_JIT_DIRECT_REGS|direct_regs_enabled|WIE_JIT_TAILCHAIN|tail_chain_enabled|return_call|return_call_indirect' crates || true)
test -z "$matches"
```

Expected: no current source references.

- [x] **Step 2: Run formatting, lint, and workspace tests**

```bash
./scripts/check-file-sizes.sh
cargo fmt --all --check
cargo clippy --workspace --all-targets
cargo nextest run --workspace
```

Expected: all pass. Do not remove or skip any matrix axis to make this cleanup pass.

- [x] **Step 3: Run the JIT/interpreter differential checks**

```bash
WIE_JIT_CHAIN=0 cargo nextest run -p wie-cpu -E 'test(jit::pipeline::tests) or test(jit::profile::tests) or test(jit::cache_persist::tests) or test(jit::lower)'
WIE_CPU=iced cargo nextest run -p wie-cpu -E 'test(simd_) or test(x87_)'
```

Expected: both paths remain green. The first filter deliberately excludes chain/invalidation tests because `WIE_JIT_CHAIN=0` disables the capability those tests assert; chain behavior is covered by the default full suite.

- [x] **Step 4: Run the micro-suite and release performance smoke**

```bash
make -C micro-exes
./scripts/run-micro-suite.sh
cargo build -p wie-cli --release
./scripts/run-micro-suite.sh long_loop
```

Expected: micro-suite passes and the `long_loop` smoke remains within the documented release timing range. This cleanup must not regress the shipped nested-call path.

- [x] **Step 5: Final diff and documentation review**

```bash
git diff --check
git status --short --branch
```

Confirm the diff contains only the two JIT source files plus the explicitly listed current-status documentation, with no changes to live capture/headless fallbacks or interpreter/degrade code. Leave the change uncommitted for review.

---

## Self-Review Checklist

- Spec coverage: Tasks 1–3 cover the inert direct-register flag, unusable tail-call branch, current docs, and validation.
- Placeholder scan: no unspecified code or test steps; all file paths and command expectations are explicit.
- Type consistency: both removed fields are private config state with no external consumers; `emit_chain_or_exit` keeps its existing signature and nested-call path.
- Oracle preservation: live chain, SSA flags, memory/SIMD/string switches, iced interpreter, and degrade-not-die remain untouched.
