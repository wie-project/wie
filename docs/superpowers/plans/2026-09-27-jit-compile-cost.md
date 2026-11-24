# JIT Compile-Cost Policy Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cut JIT compile cost so real tools start and run materially faster, without regressing compute-bound steady-state throughput.

**Architecture:** The JIT currently compiles every block at Cranelift `opt_level = "speed"`, which costs ~5.5 ms per block and starves the background compiler. Make cheap compilation the default, add a hotness-driven tier-up to `speed` for blocks that earn it, and make the opt level part of the persistent-cache identity so a cached `none` block can never be served to a `speed` run.

**Tech Stack:** Rust, Cranelift 0.133 (`cranelift_codegen::settings`), existing `JitConfig` env-knob layer, existing bg-compile worker pool, existing on-disk code cache.

**Spec:** `docs/implementation-plan.md` (Wave 3) and `docs/status.md`. The perf goal is the plan's stated one: high-FPS emulation of real apps.

## STATUS (2026-09-27)

- **Task 1 — DONE** (`9930935`): cache key now `jit_cache_key(pe_hash, opt_level)`, `FORMAT_VERSION` 1 → 2 so v1 ledgers are deleted not orphaned, `load_file` refuses a key/opt mismatch. 3 tests.
- **Task 2 — DONE** (`e2c7d5c`): `OPT_LEVEL_DEFAULT = "none"`, parsing extracted to a pure `opt_level_from_env(Option<String>) -> &'static str`. 4 tests.
- **Task 3 Part A — DONE** (`2c70770`): the ledger is record-level opt-aware. `LedgerRec.compiled_at_opt: OptTier` (new `jit/tier.rs`; unknown codes rejected, never guessed), `FORMAT_VERSION` 2 → 3, record-level rejection in `load_file`, and two-ledger routing — tier records go to a second key-derived file the writing run never reads back, and an all-empty tier ledger is not written at all, which preserves Task 1's "a `none` run leaves the `speed` namespace alone" invariant. 9 new tests; free on `long_loop` (the 1.36x ratio reproduced).
- **Task 3 Part B — DONE** (`b6db764`). The two hazards this plan predicted were real and are designed around rather than discovered later. **Route:** one `JITModule` per tier, because the single-module `TargetIsa::flags()` lever would require an `unsafe` transmute of a `#[repr(Rust)]` field — a layout assumption with no API guarantee, to save one module allocation. **The correctness hazard was neutralised by an invariant, not avoided:** direct chaining is same-tier-only, so no `FuncId` ever crosses a module and a cross-tier edge falls back to the late-bound chain hop / dispatcher. Bite proof of that invariant: removing the filter does not panic — it turns a 5-trip loop into one that **hangs past 60 s**, which is strictly worse than a crash. **Outcome:** `long_loop` recovers to **0.98× of the `speed` baseline** (0.240/0.249 s vs 0.239/0.242 s) from 0.323/0.335 s, so the ~38% compute-bound price of the cheap-compile default is gone. 7-Zip Extra pays **+2.8% warm** for 8 speculative tier compiles it never repays — bounded by `WIE_JIT_TIER_BUDGET` (default 8), with `WIE_JIT_TIER=0` verified against a pre-change binary. **This plan's "7-Zip has no self-loops" datum was WRONG:** it has **18** self-loops; what has none is its *visit-hot* set — and the budget is sized against self-loops, so the distinction matters.
  **The block that was closed:** The blocker was **Cranelift, not policy**, and is now closed. `cranelift_jit::JITModule` binds one `TargetIsa` (hence one `opt_level`) in a private field with no setter, and the in-module lever `TargetIsa::flags()` was unreachable because `isa::Triple` and `CompiledCodeStencil` are not publicly re-exported. A direct `target-lexicon = "0.13"` dependency (already in the lock at 0.13.5 as a Cranelift transitive) closes it.
  **Do NOT take the "second `JITModule` per tier" route** — it is a correctness hazard, not a workaround. `JitShared::chain_ids` is a flat `VA → FuncId` table, and `Module::declare_func_in_func` returns `ir::FuncRef`, not `Option<FuncRef>`, indexing that module's own `compiled_functions[func_id]`. A cross-module id either panics or silently names a different function and emits a call to the wrong address.
  **This plan's earn signal was wrong and is corrected here.** The hotness counter cannot be it. Measured `long_loop` is `block_entries=1`, `cache_hits=1`, `hot=0`, with 1.1e9 instructions retired inside a *single* dispatcher entry, so there is no post-compile observation before it has retired everything it ever will. And `Hot { visits, thr }` is destroyed at promotion (the entry becomes `Ready`) while promotion fires on the first visit with `visits >= thr` — so `visits` can never exceed `thr` by a margin, and the "exceed the threshold by a clear margin" guardrail is **not expressible on that counter at all**. Tiering must therefore be decided **pre-compile, from block shape** (`is_loop`): a block whose terminator jumps to its own entry multiplies emitted-code quality by its iteration count, while every other shape amortises it over at most one pass plus dispatch overhead. Supporting datum: 7-Zip's `hot` set contains no self-loops (`WIE_JIT_LOOP_HOTNESS=8` vs `=1000000` gives identical `hot=15`), and its `insn_per_entry=215` is one-shot-ish.
- **The plan's original causal claim was WRONG and is corrected below.** The first measurement compared a *cold-ledger* `speed` run against a *warm-ledger* `none` run.

## Corrected measured evidence (2026-09-27, release, interleaved A/B, medians)

Host: 8-core Apple Silicon, shared (load avg 7–22), so arms are interleaved pair-by-pair and reported as min/median — interference can only add time.

**7-Zip Extra `7za.exe i --max-api 400000`**, split by ledger warmth, because conflating them was the original error:

| metric | COLD `speed` | COLD `none` | WARM `speed` | WARM `none` |
| --- | ---: | ---: | ---: | ---: |
| `emu_ms` median | 3783 | **2590** | 1723 | **989** |
| speedup vs `speed` | — | **1.46x** | — | **1.74x** |
| `compile_us` median | 7,680,801 | **5,115,307** | 1,890,008 | **987,747** |
| compile reduction | — | **1.50x** | — | **1.91x** |
| `eager` | 987 | 987 | 987 | 987 |
| `hot` | 209 | 209 | 65 | 65 |
| `bg_hit` | **0** | **0** | 739 | **849** |

**`long_loop`** (pure compute — the regression gate), 10 interleaved pairs, non-overlapping distributions:

| arm | min | median | max |
| --- | ---: | ---: | ---: |
| `WIE_JIT_OPT=speed` | 0.310 s | 0.330 s | 0.360 s |
| default (`none`) | 0.430 s | 0.450 s | 0.530 s |
| ratio | **1.39x slower** | **1.36x slower** | |

**The real mechanism (simpler and better than the original claim):** the foreground `eager` compile **count is unchanged at 987** in both arms. The entire win is *per-compile cost*, roughly 2x cheaper. It is not fewer compiles.

**Background compiles are NOT starved at `speed`.** `bg_hit` is 0 on a cold ledger in *both* arms — opt level is irrelevant there. Warm, `speed` already lands a median of 739 of ~1000 jobs and `none` reaches 849: a **~15% effect, not 0 → 751**. Any future write-up must not repeat the "1142 wasted jobs" framing.

**Price:** compute-bound steady state regresses ~38% (`0.300 s → 0.415 s` median). The real pin lived in `docs/status.md:34` (not `CONTRIBUTING.md`, as this plan originally claimed) and moved to `~0.40–0.55 s`.

**Therefore the goal is not "pick one opt level"** — it is "stop paying `speed` for blocks that run once, and pay it only for blocks that prove hot."

## Global Constraints

- Workspace lints deny `unwrap_used`/`expect_used`/`panic`/`unreachable`/`todo`/`unimplemented`/`indexing_slicing`/`as_conversions`, all cast lints, and `clippy::pedantic`; `unsafe` is confined to `wie-cpu`. No new `#[allow]`.
- `scripts/check.sh` is the authoritative pre-PR gate: `fmt --check`, clippy (advisory), `cargo nextest run --workspace`, micro-suite.
- All timing on **release** builds only. `CONTRIBUTING.md` pins `long_loop` at ~0.25–0.32 s release JIT; changing the default opt level moves that number and the pin must be updated in the same change, not left to rot.
- The persistent JIT cache (`WIE_JIT_CACHE`, on by default) must never serve code compiled under a different opt level.
- Any behaviour change needs a bisect switch (an env knob) so a regression can be attributed.

---

### Task 1: Make the persistent cache opt-level-aware (correctness first)

This is first because it is a latent *correctness* bug the moment opt level becomes variable, and it must be true before Task 2 introduces variability.

**Files:**
- Modify: `crates/wie-cpu/src/jit/cache_persist.rs` (cache key derivation)
- Test: `crates/wie-cpu/src/jit/tests/` (new test module, or the existing cache tests)

**Interfaces:**
- Consumes: `JitConfig::get().opt_level()` (`config.rs:376`) returning `&'static str` (`"none" | "speed_and_size" | "speed"`).
- Produces: the cache key includes the opt level, so a cached artifact is only reused under the opt level it was compiled with.

- [ ] **Step 1: Read the current cache key derivation and confirm the hazard**

  Find how `cache_persist.rs` builds its key/hash. Confirm that two runs with different `WIE_JIT_OPT` would currently collide. Record the exact function and line.

- [ ] **Step 2: Write the failing test**

  A test that compiles/artifacts a cache entry under one opt level and asserts it is NOT returned under a different one. If the current key function is a pure function, assert directly on the derived key for two different `opt_level` values.

- [ ] **Step 3: Run it to confirm it fails**

  Run: `cargo nextest run -p wie-cpu -E 'test(cache)'`
  Expected: FAIL — the key does not vary with opt level.

- [ ] **Step 4: Fold the opt level into the key**

  Mix `JitConfig::get().opt_level()` into the key derivation. Keep it a pure function if it is one today; do not introduce I/O.

- [ ] **Step 5: Run the test again; confirm green**

  Run: `cargo nextest run -p wie-cpu -E 'test(cache)'`
  Expected: PASS.

- [ ] **Step 6: Verify an end-to-end cache round trip**

  Run a micro-exe with `WIE_JIT_CACHE=dir` and `WIE_JIT_OPT=none`, then again with `WIE_JIT_OPT=speed`, and confirm the second run does not reuse the first's artifact. Report the on-disk entries observed.

- [ ] **Step 7: Commit**

```bash
git add crates/wie-cpu/src/jit/cache_persist.rs crates/wie-cpu/src/jit/tests
git commit -m "jit: key the persistent code cache on opt level"
```

---

### Task 2: Default to cheap compilation, with a bisect switch

**Files:**
- Modify: `crates/wie-cpu/src/jit/config.rs` (the `opt_level` default, currently `_ => "speed"` at ~`:252`)
- Modify: `docs/RUNBOOK.md` (knob table), `docs/status.md`, `CONTRIBUTING.md` (the `long_loop` pin)
- Test: `crates/wie-cpu/src/jit/config.rs` tests (there are existing config-parsing tests)

**Interfaces:**
- Consumes: nothing new.
- Produces: `WIE_JIT_OPT` default `"none"`; `WIE_JIT_OPT=speed` remains the documented opt-out for compute-bound guests.

- [ ] **Step 1: Write the failing test**

  A config test asserting the default `opt_level()` is `"none"` when `WIE_JIT_OPT` is unset, and still `"speed"` when explicitly set.

- [ ] **Step 2: Run it; confirm it fails**

  Run: `cargo nextest run -p wie-cpu -E 'test(config)'`
  Expected: FAIL — the default is currently `"speed"`.

- [ ] **Step 3: Flip the default and document the trade-off in the code**

  Change the fallback arm. The comment must state the measured trade-off in one line so the next reader does not "fix" it back: real tools get ~2.4x faster because compile cost dominates them, while a pure-compute loop pays ~40%.

- [ ] **Step 4: Confirm green, then re-measure the two workloads**

  Run: `cargo nextest run -p wie-cpu`, then release timings for `long_loop` and `7za.exe i`. Report both, and state plainly that `long_loop` regresses under the new default.

- [ ] **Step 5: Update the `CONTRIBUTING.md` pin to the measured value**

  The pin exists to catch regressions, so it must reflect reality. Record the new measured `long_loop` range and note that the compute-bound regression is the known cost of the cheap-compile default, to be recovered by Task 3.

- [ ] **Step 6: Commit**

```bash
git add crates/wie-cpu/src/jit/config.rs docs/RUNBOOK.md docs/status.md CONTRIBUTING.md
git commit -m "jit: default to opt_level=none - compile cost dominates real tools"
```

---

### Task 3: Tier hot blocks up to `speed` (recovers the compute-bound case)

> **PREREQUISITE — do this first, it is a correctness fix, not an optimization.**
> Task 1 keyed the cache on the **run-level** opt level, which is correct for
> Tasks 1–2 because one run compiles everything at one level. Per-block tier-up
> breaks that assumption: one process will compile the same guest VA at **both**
> `none` and `speed`. A per-file key is then wrong, and a `none` run will happily
> load a `LedgerRec` for a block that only ever succeeded at `speed`. Add a
> per-record `compiled_at_opt` field to `LedgerRec`, bump `FORMAT_VERSION` to 3,
> and have the load path reject a record whose `compiled_at_opt` does not match
> the level the block is being compiled at now. Only then implement tier-up.

**Anti-thrash guardrails (a tier-up policy that oscillates is worse than none):**
- **Tier up only, never down.** Once a block is compiled at `speed` it stays there
  for the rest of the run. A block that oscillates across the threshold would
  otherwise recompile forever, and recompiling is exactly the cost Task 2 removed.
- **At most one tier-up per block per run.** Assert it.
- **Bound the total**: cap tier-ups per run (or per guest) so a pathological
  workload cannot spend the compile budget re-establishing `speed` everywhere.
- **A block must earn it**: require the existing hotness counter to exceed
  `hotness_threshold()` by a clear margin, not merely touch it.

**Files:**
- Modify: `crates/wie-cpu/src/jit/cache_persist.rs` (the `compiled_at_opt` prerequisite), `crates/wie-cpu/src/jit/pipeline.rs` (the promotion path that already produces `hot=209` compiles), `crates/wie-cpu/src/jit/engine.rs` (per-compilation `settings::Flags` instead of one shared set), `crates/wie-cpu/src/jit/config.rs` (threshold/knob)
- Test: `crates/wie-cpu/src/jit/tests/`

**Interfaces:**
- Consumes: the existing hotness counter and promotion signal that already produces `hot=209` compiles; `JitConfig::hotness_threshold()` (`config.rs:281`).
- Produces: a block that exceeds the hotness threshold is recompiled at `opt_level = "speed"`; the recompile is charged to the existing `profile.compile_us` accounting, and a knob (`WIE_JIT_HOTNESS_THRESHOLD`) bounds it.

- [ ] **Step 1: Read the promotion path and confirm a recompile point exists**

  Find where a block crossing the hotness threshold triggers a compile today, and confirm a second compile of the same guest address is representable (cache invalidation / `code_invs` accounting). If a block cannot currently be recompiled, say so and stop — this task depends on it.

- [ ] **Step 2: Write the failing test**

  A test that runs a hot loop long enough to cross the threshold and asserts (a) the block is compiled more than once, and (b) the second compile used `speed`. Assert on observable state, not on a private flag.

- [ ] **Step 3: Run it; confirm it fails**

  Run: `cargo nextest run -p wie-cpu -E 'test(hotness) or test(tier)'`
  Expected: FAIL — every compile currently uses the single configured opt level.

- [ ] **Step 4: Implement the tier-up**

  Build `settings::Flags` per compilation rather than once, selecting `speed` for tier-up compiles. Keep Task 1's cache key authoritative: a `speed` artifact must never be served for a `none` expectation or vice versa.

- [ ] **Step 5: Confirm the trade-off actually inverts**

  Re-measure `long_loop`: it should recover toward the `speed` baseline, because its loop is hot enough to tier up. If it does not recover, the threshold is too high or the promotion signal does not fire for a self-loop — diagnose and report rather than lowering the threshold blindly.

- [ ] **Step 6: Re-verify 7-Zip did not regress**

  `none` must still win for `7za.exe i`; a block that runs once must never tier up. Report both numbers.

- [ ] **Step 7: Commit**

```bash
git add crates/wie-cpu/src/jit
git commit -m "jit: tier hot blocks up to opt_level=speed"
```

---

### Task 4: Re-baseline and document

**Files:**
- Modify: `docs/status.md`, `docs/implementation-plan.md` (Wave 3 rows), `docs/RUNBOOK.md`
- Regenerate: `docs/baselines/` perf rows if the harness writes them

- [ ] **Step 1: Record the final measured matrix**

  A table of `7za.exe i` and `long_loop` under the shipped default and under `WIE_JIT_OPT=speed`, plus `compile_us`, `bg_hit`, and `emu_ms` for each.

- [ ] **Step 2: State what was traded**

  Write down explicitly that the cheap-compile default costs compute-bound steady state and is recovered by tier-up, and what happens if tier-up is disabled.

- [ ] **Step 3: Run the full gate**

  `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets`; `cargo nextest run --workspace`; `make -C micro-exes && ./scripts/run-micro-suite.sh`; `./scripts/test-fast.sh`. All must be green, with counts.

- [ ] **Step 4: Commit**

```bash
git add docs
git commit -m "docs: record the opt-level policy and its measured trade-off"
```

---

## Out of scope (deliberately)

- **Tail-call chaining / the direct-register ABI.** Measured saturated on 2026-09-26 (`WIE_JIT_CHAIN=0` is not slower). Not part of compile cost.
- **`enable_verifier`.** Currently `"true"` (`engine.rs:77`), which is a real per-compile cost and a plausible follow-up, but it is a correctness guard; changing it needs its own evidence and its own bisect switch.
- **Chain `resyncs`** (~700–980 on 7-Zip). Re-measured after Task 2 and **unchanged** by opt level: 755 → 707 cold (6% fewer, within noise) and 978 → 980 warm (identical). They track **ledger warmth**, not compile cost — cold runs sit at 700–770, warm at 940–985. The original "second-order effect of slow compiles" hypothesis is **refuted**; re-file this under ledger seeding, not under opt level.
- **Shader / D3D9 pipeline work.** Unrelated to JIT compile cost.
