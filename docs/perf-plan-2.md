# Runtime Performance Plan — Round 2

Status: Active · 2026-08-24 (revised after skip-depth=4 validation + 7za corpus probe)
Predecessor: docs/perf-plan.md (Painpoints 1–2 landed). Protocol: nextest everywhere,
debug builds until done, one 40 s SIGINT release benchmark per validation round.

---

## 0. Current state (measured)

| Fact | Evidence | Consequence |
| --- | --- | --- |
| Stall tax eliminated | `bg_stall_us` 5 597 ms → 245 ms; `bg_to` 3 105 → 65 (`BG_WAIT_SKIP_DEPTH=4`) | Guest thread never meaningfully blocks on compiles |
| Throughput ×3.18 | 1 194 M insns @ same handler volume as baseline (~138 K allocs) | Real execution gain, not workload drift |
| Iced is the #2 cost | 80.2 M interpreted insns (6.7 %); ≈ **7 s of 40.5 s emu budget** at documented ~11 M steps/s | Biggest remaining addressable compute sink |
| Worker saturated | 26 001 promotions vs 10 253 compiles; `compile_us` 12.8 s; cpu % 130.5 | Cooldown churn inflates promotions ×2.4; capacity is the ceiling |
| Opcode cliff is fatal | 7za dies: `unimplemented mnemonic Movlpd` at msvcrt `fputs` call site — interpreter has NO path, so it is an **error**, not a slowdown | Real-app corpus blocked by correctness, not speed |
| Idle unvalidated | All captures still `idle_policy=yield`; noisy exits ~306 K | P1 landed but its benefit never measured |
| Micro-suite regression open | Earlier `run_micro … 40 slices` failure report, unresolved | Exe-level gate is local-only; CI cannot catch it (ci.yml has no micro-suite) |
| Two anomalies unexplained | `winapi_lock_wait` max 46 µs → 881 µs; `closehandle` ≈ 2 ms/call | Sub-0.1 % today; must be attributed before they grow |

Dispatch efficiency recovered: 8.58 insns/cache-hit (from 6.6). Handler layer remains a
non-cost (0.3 %).

---

## Goals (ordered)

### G1 — Kill the opcode cliff (correctness first)
Real exes must not terminate on unknown mnemonics. The interpreter's unimplemented set
is the enemy: every mnemonic implemented removes a crash class **and** flips whole
NotPure blocks to Pure (perf superlinearity).
- Enumerate: grep the exec layer's unimplemented-mnemonic construction; cross-reference
  the sampled opcode histogram (`WIE_JIT_OPCODE_HISTO=1`, long-running targets only —
  micro-exes exit instantly and the SIGINT path never arms).
- Batch 1 candidates: the packed-double lane-move family (`movlpd`, `movhpd`,
  `movs[d]` load/store forms) — trivial single-lane lowers in `jit/lower/sse_fp.rs`
  precedent, and `movlpd` alone unblocks 7za.
- Acceptance: `wie run real_exes/7za.exe b` survives ≥ 60 s; full real-exe corpus
  (7za, notepad, 2048, doomretro) launches without `unimplemented mnemonic`.

### G2 — Iced share < 1 %
From 6.7 %. Driven by G1 plus, if histograms show mixed blocks dominating,
block splitting behind `WIE_JIT_SPLIT=1` (pure prefixes/suffixes compile; matrix-gated).
Acceptance: ≤ 12 M interpreted insns per 1.2 B-insn capture; targeted micro walls drop
proportionally.

### G3 — Compile capacity ≥ promotion demand
End the promote→defer→cooldown churn (26 K vs 10.3 K).
- C3 spike (read-only, timeboxed): are inter-block references runtime-resolved chain-table
  slots (multi-module-safe) or direct JIT symbols? If runtime-resolved → K workers, each
  owning a private `JITModule`+`Context`; shared cache/epoch protocol unchanged;
  expected effective compile latency ÷K.
- Fallback (no code): raise `WIE_JIT_TARGET_WORK` 900 → 1800/3600 to cut demand; measure
  both sides on release.
Acceptance: deferred/cooldown outcomes < 10 % of enqueues; steady-state worker CPU
< 25 % of one core after startup burst.

### G4 — Prove P1 under park
Every future capture uses `WIE_IDLE=park`. Acceptance: noisy exits collapse (~306 K →
≈ 0); resting CPU < 5 %; `idle_residency_ms` counter attributes ≥ 90 % of former spin
wall; input-to-first-message < 5 ms. Console-path park already migrated (`run.rs`);
verify SIGINT-under-profiling still prints (50 ms liveness caps preserved).

### G5 — Dispatch efficiency: insns per cache-hit ≥ 9 sustained
Land chaining coverage counters first (link attempts / linked / blocked-by-not-ready /
blocked-by-cap / epoch-resync count). Then, data-driven:
- If H1 (successors not Ready at link time) dominates → install-time incremental linking
  via decoded predecessor sets.
- If H2 (O(cache) resync per epoch bump) dominates → generation-delta resync.
- Superblocks across unconditional jmp edges behind `WIE_JIT_TRACE=1` (cap 96 → 192),
  matrix-gated.

### G6 — Regression safety net
- Reproduce and fix the open micro-suite `40 slices` failure (selective-stash bisect
  across Lane A/B/integration edits if needed).
- Add the exe-level micro-suite to CI as a nightly job (it is currently local-only —
  the reason the regression reached us late). Gate G1 on it too.
- Attribute the two anomalies (lock-wait max spike ≈ resync/install window?;
  closehandle latency) with one instrumentation pass each; fix only if they grow.

Non-goals this round: handler layer / heap / VFS (0.3 % combined), TLB memory fast path
(slow-path counters small), register-spill-window surgery (deep; revisit only if G2/G5
leave insn rate short).

---

## Required changes → files

| Change | Files | Goal |
| --- | --- | --- |
| Lower `movlpd`/`movhpd` (+histogram top-N) | `jit/lower/sse_fp.rs`, `lower/insn.rs` | G1, G2 |
| Block splitting (conditional) | `jit/block*.rs`, decode classification | G2 |
| Multi-module workers OR target-work retune | `shared.rs`, `config.rs` | G3 |
| Chaining counters + incremental link/resync | `pipeline.rs`, `shared.rs`, `mod.rs` stats | G5 |
| Splitting/trace knobs default flip | `config.rs`, RUNBOOK | G2, G5 |
| Nightly micro-suite CI job | `.github/workflows/` | G6 |
| Park-mode validation + committed baselines | scripts/capture-baseline.sh runs, `docs/baselines/` | G4 |

## Waves

1. **Wave 0** (now): micro-failure bisect; chaining counters; histogram captures on
   long-running targets (7za post-G1-batch-1, doomretro, notepad+input script).
2. **Wave 1**: G1 batch-1 lowers → 7za unblocked; re-rank histogram; nightly CI lands.
3. **Wave 2**: G3 spike → decision; G5 fixes per counter data; G2 split decision.
4. **Wave 3**: release knob matrix (`TARGET_WORK` × `OPT` × workers), park-vs-yield
   paired captures committed to `docs/baselines/`, defaults + docs/status refresh.

## Validation

Per wave: workspace nextest green; `cfg(test)` eager-everything determinism untouched.
Final: 40 s SIGINT release benchmark, paired `WIE_IDLE=yield|park`, diffed against
committed baselines. Targets: stalls < 250 ms held · iced < 1 % · insns/hit ≥ 9 ·
noisy ≈ 0 under park · `long_loop` within ±3 % · real-exe corpus launches clean.

## Risks

- Lowering FP-lane moves wrong (alignment/`#gp` semantics differ per form) → each
  mnemonic gets a unit test against known-good bytes before landing.
- Multi-module workers widen SMC invalidation surface → epoch broadcast is the choke
  point; cross-module invalidation test required before any default flip.
- Incremental chain patching writes live code addresses → patch only validated-Ready
  successors, mirroring epoch-resync safety.
