# Runtime Performance Plan — hot-path painpoints and fixes

Status: Proposed · 2026-08-24
Scope: execution-loop performance only (`emu_ms` = 99.7% of runtime). WinAPI handler layer,
guest heap, VFS, and marshaling are explicitly out of scope (< 0.3% combined).
Evidence base: `WIE_RUNTIME_PROFILE=1` capture of a GUI workload plus code audit
(file:line anchors below). No implementation yet — this document is the plan.

---

## 1. What the baseline profile says

| Metric | Value | Reading |
| --- | --- | --- |
| Wall / CPU | 41.07 s wall, 96.9 % CPU, sys only 0.53 s | User-space burn; `idle_policy=yield` ⇒ suspected busy-spin during guest idle |
| Emulation vs handlers | `emu_ms` 40 749 (99.7 %) vs `handler_ms` 118 | The JIT/memory loop dominates; handler layer is a non-issue |
| Host stops | 613 952 total, 301 518 noisy, 5 458 charged | Half the exits do no attributable work — consistent with poll/idle exits |
| Throughput anomaly | 369.6 M insns over 40.7 s ≈ 9 M/s average | `long_loop` sustains ~350 M/s (docs/status.md); most wall time is *not* instruction execution |
| Background JIT | `bg_stall_us` 2 922 ms, `bg_to` 2 080, `bg_enq` 10 863 | ~7 % of wall lost stalled on the compile queue; timeouts then inline-compile anyway |
| Compile cost | `compile_us` 13.9 s across ~12 935 compiles ≈ 1.08 ms/block | Single worker cannot drain promotion bursts |
| Iced fallback | 5.83 M stepped insns (1.58 %) at ~11 M steps/s ≈ 0.53 s | Whole blocks interpret whenever one instruction is non-lowerable |
| Non-painpoints | heapalloc/free ≈ 0.28 µs/call; lock waits ≤ 46 µs; ReadFile/VFS trivial | Do not spend effort here |

Key inference: at documented JIT throughput the captured 369 M insns represent roughly
1–3 s of real compute inside 40.7 s of wall. The missing tens of seconds must be idle
spin / poll loops — currently unmeasured directly (Phase 0 adds the counter).

---

## 2. Phase 0 — Measurement harness (prerequisite)

The repo has **zero benchmarks** (no criterion/hyperfine anywhere). CLAUDE.md claims
perf regressions fail PRs; nothing enforces it. Every change below is gated on this phase.

1. **Reproducible baselines.** Re-run the profiled app under `WIE_IDLE=yield` (as
   captured) and `WIE_IDLE=park`, driven by the existing input-script knob; commit both
   `WIE_RUNTIME_PROFILE=1` JSON dumps.
2. **Three new counters** (cheap, always-on or env-gated):
   - *Idle residency ns* — wall time in `WaitingForMessage` state (today unmeasured;
     expected to absorb most of the unaccounted wall).
   - *Sampled iced-step opcode histogram* — which opcodes force interpretation.
   - *Promotion outcome ledger* — per enqueue: hit-as-ready / stalled / timed-out / cooled-down.
3. **First criterion benches** in `wie-cpu`: `long_loop` throughput, block-dispatch
   overhead, TLB translate latency, compile throughput.
4. Wire benches into a **nightly CI job** (not per-PR initially).

Exit criteria: baseline JSONs committed; idle-spin share quantified.

---

## 3. Painpoint 1 — Idle: event-driven park via per-thread channels

### Root cause (anchored)

- Captured run executed under `yield`: empty `GetMessage` returns
  `WinApiControlSignal::WaitingForMessage` with no sleep → outer loop spins at ~97 % CPU.
  (`crates/wie-winapi/src/idle.rs:11-20`, pump paths in
  `crates/wie-runtime/src/session/pump.rs`.)
- Even the persistent default `Park` is polling, not blocking:
  `apply_message_park` = `thread::sleep(25 ms)` quantum (`idle.rs:129-131`), capped at
  40 quanta ≈ 1 s (`idle.rs:97-98`); `WaitForSingleObject/Multiple(INFINITE)` polls in
  50 ms slices (docs/architecture/runtime.md). Worst-case input latency 25 ms by construction.

### Design — one wake primitive, reused everywhere

Topology: **every channel is per-thread; fan-out lives in explicit registries.**

1. **One std `mpsc` inbox per guest thread.** All wake sources send a token into some
   thread's inbox:
   - PostMessage / cross-thread SendMessage → target thread's inbox;
   - GUI input injection → focused window's owning thread (winit handler sends);
   - timer arm → owner thread;
   - synchronous `SendMessage` reply → *sender's* inbox (covers the interlock);
   - thread teardown → explicit `Wake::Shutdown` token.
2. **Per-object waiter registry**: `Mutex<Vec<ThreadId>>` touched only at wait-enter,
   wait-exit, and signal — never held across a park (preserves the documented deadlock
   rule, docs/architecture/runtime.md).
   - Manual-reset event / `ReleaseSemaphore(n)` → token to **all** registered threads.
   - `ReleaseMutex` / auto-reset event → registry selects **exactly one** acquiring
     thread; one token there. Deliberate selection matches Win32 semantics; a shared
     competing-consumer channel would let an arbitrary waiter steal the wake.
3. **Park procedure:** re-check queue/state → if still idle, `recv_timeout(deadline =
   next due timer)` → drain inbox (`try_recv` loop) → re-evaluate → run or re-park.
   Tokens are **hints, never data** — dropping or duplicating one degrades to a spurious
   wakeup, never a lost one. State stays in the queue/timer structures it lives in today.
4. **Delete the 40-quanta cap** — it is a safety valve for sleep-polling; `Shutdown`
   tokens make teardown explicit instead.
5. **Migrate `WaitFor*` off 50 ms poll slices** onto the same primitive; `INFINITE`
   becomes plain `recv()`.
6. Micro suite keeps `Yield`; deterministic tests keep `ExitOnIdle`. Pure guest spin
   loops are never parked (existing design invariant, idle.rs header — correct, keep).

No crossbeam: MPMC never arises once wakes are per-thread. Revisit only if a true
competing-consumer pool appears (e.g., N-worker shared compile queue) — YAGNI today.

### Validation

Resting CPU < 5 % (from 96.9 %); input→first-message < 5 ms (from ≤ 25 ms worst case);
idle-attribution counter explains ≥ 90 % of previously unaccounted wall.

### Risks

Missed-wakeup races → drop-token invariant + stress test with concurrent input
injection from the winit thread while a guest drains its inbox. Guests polling
non-message mechanisms legitimately stay busy — by design.

---

## 4. Painpoint 2 — Background JIT: stop paying twice for the same block

### Root cause (anchored)

- Single background worker (`ensure_bg_worker`, `crates/wie-cpu/src/jit/shared.rs:243`)
  draining a 1024-slot queue (`BG_QUEUE_CAP`, `jit/config.rs:20`) at ~1.08 ms/block.
- Promotion threshold is flat visit-count 100 (`hotness_threshold_from_env`,
  `config.rs:317-326`); bursts of promotions outrun the worker.
- Guests re-enter `Queued` entries → `wait_bg_ready` stalls in 1 ms-chunk condvar waits
  (`pipeline.rs:503-551`) averaging 1.15 ms × 2 536 = 2.92 s.
- On timeout the main thread **inline-compiles the same block the worker is already
  compiling** (`pipeline.rs:313-318`, 389-394): the stall bought nothing, the block is
  built twice, and the two threads contend.
- Threshold ignores block size: a 9-insn block and a 96-insn block have ~10× different
  payback horizons but identical thresholds. Eager-by-size (`WIE_JIT_EAGER_BLOCK_INSNS=48`,
  `config.rs:334-339`) forces first-sight compiles whose own rationale fails arithmetic:
  a one-shot 96-insn block costs ~9 µs interpreted vs ~1 ms compiled — a 100× loss
  whenever it does not revisit.

### Design

1. **Work-weighted promotion.** Replace flat visit count with
   `thr(insns) = clamp(TARGET_WORK / insns, floor, ceiling)` — promote when accumulated
   interpreted work exceeds predicted compile cost × margin.
2. **Drop eager-by-size as a default.** Restrict eager compiles to fast-UCRT and
   self-loop blocks; keep the env knob for diagnosis.
3. **Timeout → Cooldown, not inline compile.** On stall timeout, re-insert the entry as
   `Hot` with a doubled threshold (hysteresis, capped) and keep interpreting. Inline
   compilation is reserved for worker-dead/unavailable. Deletes the double-compile and
   converts hard stalls into distributed interpretation.
4. **Throughput:** persistent/reused Cranelift context on the worker + batched queue
   drain; target median ≤ 250 µs/block (status.md itself cites 50–500 µs as achievable).
5. **Replace `BgWaitCell` chunked condvar polls with a one-shot completion token**
   (`recv_timeout(bg_wait_budget)` directly) — removes the 1 ms-chunk latency floor and
   unifies the wake idiom with Painpoint 1's channels.
6. Optional backpressure: promotion consults queue depth (one atomic load); deep queue
   raises the local threshold.

Explicitly rejected: cache eviction/demotion of cold compiled blocks. ~13 k entries is
negligible memory; eviction solves a non-problem (YAGNI).

### Validation

Total `bg_stall_us` < 100 ms (from 2 922 ms); `bg_to` ≈ 0; median compile ≤ 250 µs;
`long_loop` within ±3 %.

### Risks

Cooldown oscillation → hysteresis doubling handles it; extend
`hot_threshold_crossing_compiles_and_invalidates` (`jit/mod.rs:652`) to cover
cooldown × invalidation interleavings.

---

## 5. Painpoint 3 — Iced wedge: shrink the interpreted residue

### Root cause (anchored)

`decode_pure_gpr_block` classifies a whole block NotPure on the first non-lowerable
instruction — one exotic opcode poisons up to 95 perfectly lowerable ones (block cap
96 insns, docs/architecture/cpu-and-memory.md). Direct cost ≈ 0.53 s; block-level
opportunity cost wherever such blocks are hot.

### Design

1. Rank offenders using Phase 0's opcode histogram (share × block frequency;
   candidates per capability matrix: x87, SSE gaps, REP variants beyond inlined ones,
   atomics).
2. Lower top-N opcodes in `jit/lower/*` following the established module pattern
   (`lower/sse_fp.rs` precedent). Each conversion flips **whole blocks** back to Pure —
   superlinear win per instruction.
3. If histograms show mixed blocks dominating: **block splitting** at non-pure
   instructions so pure prefixes/suffixes compile. Land behind `WIE_JIT_SPLIT=1`;
   flip the default only after `scripts/check-jit-matrix.sh` passes.
4. Accept the residual: system/rare opcodes stay iced forever — the interpreter remains
   the correctness oracle.

### Validation

Iced share < 0.3 % of insns (from 1.58 %); targeted micro-exe wall improvements match
predictions.

### Risks

Splitting interacts with SMC pending-write drain and hooks → knob-gated rollout, matrix
validation before any default flip.

---

## 6. Sequencing and ownership

| Order | Lane | Files | Depends on | Parallel-safe with |
| --- | --- | --- | --- | --- |
| 0 | Measurement + baselines | profile/stats plumbing | — | — |
| 1 | Event-driven park (channels) | wie-winapi/idle.rs, runtime pump, winit bridge | P0 | P2, P3 |
| 2 | Promotion/stall rework + worker throughput | wie-cpu/pipeline.rs, shared.rs, config.rs | P0 | P1, P3 |
| 3 | Opcode lowering (+ optional splitting) | wie-cpu/jit/lower/* | P0 histograms | P1, P2 |
| 4 | CI perf gate (nightly bench + profile diff) | scripts/, .github/workflows/ | P0 | all |

Lanes 1–3 touch disjoint files; run in parallel once P0 counters exist.

## 7. Expected impact

- ~tens of seconds of idle burn eliminated; one core freed at rest (P1).
- 2.9 s guest-visible stalls removed; ~7 s redundant compile CPU recovered (P2).
- Interpreted residue cut ~5×; hot mixed blocks regain compilation (P3).
- A harness that proves each number instead of asserting it (P0/P4).
