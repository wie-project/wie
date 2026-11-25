# Compile-selection policy: what to compile, when, and at which tier

Status: **two wins shipped, one candidate refuted, one negative result recorded.**
Boot mode no longer lowers the promotion threshold (§3.1, 1.23x on `h` / 1.12x on
`x`). The threshold *shape* correction was measured and refuted (§3.1). The
persistent ledger no longer bypasses the "when" decision unconditionally: it takes
the eager path only for a block that earned one (§3.2), which is what makes a warm
boot stop being *slower* than a cold one — and its measured effect on DOOM is nil.
Decision 3 is still blocked on machinery that does not exist yet.

Scope: how a decoded guest block becomes machine code. Not lowering quality, not
dispatch cost — those are `performance-ceiling.md` levers L1/L3.

---

## 1. The three decisions

Every JIT answers three questions, and they have different inputs:

| Decision | Question | Correct input |
|---|---|---|
| **Which** | is this block ever worth compiling? | block shape (pure / has a fast call / non-pure) |
| **When** | has it executed enough to repay its compile? | hotness counter, differenced by *kind* |
| **Where** | cheap tier or expensive tier? | measured cost of each tier vs measured payoff |

wie answers **which** by decoding once into `BlockKind` and refusing non-pure
bodies outright (`mark_never`), and **when** by a per-entry visit counter
(`CacheEntry::Hot { visits, thr }`). It has no real answer to **where**: the
tier ladder exists (`tier.rs`, `OptTier::Base`/`Speed`) but is granted to
self-loops only, under a fixed budget of 8 decisions per run.

## 2. What the literature actually contributes

Named, because the design choices are not novel and should not look it:

- **HotSpot tiered compilation** — separate *invocation* and *back-edge*
  counters, because one loop entry can execute a block a million times while a
  leaf block executes once per call. wie already has this distinction as
  `is_loop` (`pure_is_self_loop`) with its own `WIE_JIT_LOOP_HOTNESS` (default
  8) versus the general threshold. That part is right and stays.
- **`CompileThresholdScaling`** — a single scalar multiplies every promotion
  threshold, so tuning is one knob, not one knob per rule. wie keeps this as
  `WIE_JIT_TARGET_WORK`.
- **Online cost models** (JikesRVM and the adaptive-compilation literature;
  HotSpot's "compilation cost" accounting) — the threshold for a tier should
  divide that tier's *measured* cost by the payoff per execution, not trust a
  constant. This is decision 2, and it is what landed.
- **V8's Ignition → Sparkplug → Maglev → TurboFan** — the cheap tier must come
  *first*, because its break-even is tens of executions rather than thousands.
  This is decision 3, and it is blocked: `jit/baseline.rs` is a stub that calls
  the full Cranelift path, so "the cheap tier" is today the expensive tier with
  a lower threshold — the opposite of the intent.
- **Counter decay / aging** (V8, HotSpot `CounterDecay`) — hotness is a
  windowed quantity, so a startup phase does not permanently bias the policy.
  Not implemented; see §5.

## 3. Decision 2: measured, and why the corrected threshold is not shipped

The shipped rule is work-weighted:

```text
thr(n) = clamp(TARGET_WORK / n, 8, 10_000)      // TARGET_WORK = 900
```

so a 9-instruction block needs ≈100 visits and a 90-instruction block ≈10. That
`1/n` shape is not a neutral heuristic — it is a *claim about compile cost*. The
break-even condition is `visits · n · s ≥ cost(n)` for a per-instruction saving
`s`, i.e.

```text
thr(n) = cost(n) / (n · s)
```

which reproduces `TARGET_WORK / n` **exactly when `cost(n)` is constant**, and
takes the shape `n^(p-1)` when `cost(n) ∝ n^p`. So the shipped model assumes
compile cost does not depend on block size.

### The measurement

`WIE_RUNTIME_PROFILE=1` now reports a compile-cost histogram
(`jit_compile_cost: insns_us/count …`, buckets `[2^i, 2^(i+1))` instructions;
see §4). One run of `7za h` on the calibrated corpus, 1 983 timed compiles:

| block insns | samples | µs / compile | cost ratio vs previous | local exponent |
|---|---|---|---|---|
| 1 | 42 | 124 | — | — |
| 2 | 716 | 242 | 1.95× | 0.96 |
| 4 | 429 | 449 | 1.86× | 0.89 |
| 8 | 217 | 809 | 1.80× | 0.85 |
| 16 | 41 | 1 483 | 1.83× | 0.87 |
| 32 | 49 | 8 284 | 5.59× | 2.48 |
| 64 | 9 | 19 081 | 2.30× | 1.20 |

**Compile cost is not flat.** Across the sizes that carry 96% of the samples
(1–16 instructions) it grows almost *linearly* — local exponent 0.85–0.96 —
and it grows faster still above 16 instructions (Cranelift's regalloc is
superlinear in body size, and `speed`-tier compiles land in the large buckets).

### What that implies

With `cost ∝ n^p`, break-even visits go as `n^(p-1)`. For the measured
`p ≈ 0.9` over 1–16 instructions that is `n^-0.1`, i.e. **essentially flat at
≈200 visits**, against the shipped `900/n` (900 visits at one instruction, 56 at
sixteen). Two independent checks agree that the engine is far more aggressive
than break-even: `WIE_JIT_OPT=none` is 1.5× faster than `speed` precisely
because compile cost dominates, and ~97% of entered blocks get compiled
(4 079 compiles for 4 216 entries on `h`).

### Why it is not in the tree

A flat ≈200-visit threshold is a *shape* change of up to 4× in one direction and
10× in the other. That is a behaviour change of exactly the kind this repo's
method requires an interleaved paired A/B to justify — and the measurement above
was taken on a host carrying load 3.2–5.7 for the whole session, which is over
the harness's 2.0 gate. Shipping it on one noisy sample would be guessing with
extra steps.

So the tree keeps: the measurement (`jit_compile_cost`), the analysis above, and
the shipped policy unchanged. The experiment is specified in §5 and is the next
thing to run.

### 3.1 What the shape experiment led to

The shape change was measured, refuted, and on the way it exposed the thing that was
actually wrong. Two experiments, both on the calibrated corpus with interleaved pairs,
alternate runs, one binary, median of per-pair ratios (arm/base, <1 is better), 5 pairs
per class, load 2.1-2.2.

**Experiment 1 — flat thresholds. Refuted.** Forcing a size-independent threshold at
225, 1 000 and 3 000 visits (4x lower to 13x higher than the shipped threshold at
typical block sizes):

| class | flat 225 | flat 1000 | flat 3000 | compiles | iced share |
|---|---|---|---|---|---|
| `x` | 1.0003 | 1.0031 | 1.0020 | 668 -> 673 | 0.024 % -> 0.040 % |
| `h` | 1.0025 | 1.0033 | 0.9992 | **identical** | unchanged |

Nothing moves: every arm is inside a +-1.5 % noise band, and the compile count does not
respond (668 -> 673). Two mechanisms explain it. On `x` the guest runs long enough that
every block which matters becomes hot whatever the threshold, so raising it only
*defers* compiles, it does not avoid them — visible in `iced`, which rises from 0.024 %
to 0.040 % of instructions, i.e. 0.016 percentage points. On `h` the counters are
identical across all four arms because boot mode forced `thr = 1` for the first 5 M
guest instructions and `h` retires 5.79 M: the threshold was **never consulted**.

That second observation is the useful one, and it is the whole of experiment 2.

**Experiment 2 — boot mode stops forcing `thr = 1`. Shipped.**

| class | arm/base | wall median | compiles | iced | guest insns |
|---|---|---|---|---|---|
| `h` | **0.811** | 1 197 -> 969 ms | 336 -> 183 | 9 114 -> 10 291 | 5.79 M both |
| `x` | **0.897** | 3 583 -> 3 210 ms | 668 -> 472 | 28 424 -> 30 026 | 120.9 M both |

**1.23x on `h`, 1.12x on `x`**, every pair favouring the change. The compile saving is
genuine rather than a shift of work into the interpreter: retired instructions are
identical, and the extra interpreted instructions are 0.02-0.03 % of the run.

Neutral on `mx9s`, by construction: boot mode is 0.27 % of that class's 1.84 G
instructions, and a cross-commit check gives a paired median of **0.998** with
byte-identical output archives and retired instructions matching to 0.003 %.

The mechanism matters, because a similar-looking change was refuted before
(`performance-ceiling.md` §11.4 A1: deleting the inline baseline made every block fall
into the promotion machinery, the queue saturated, and the engine compiled 1.9x as many
blocks). Two separate paths pull blocks into the compiler during boot, and only one of
them was earning its keep:

- blocks of **<= 32 instructions** compile on first sight in `pipeline.rs`'s boot arm,
  *before* the threshold is consulted (`baseline::is_baseline_eligible`). Untouched.
- **every other** block reached the ordinary `Hot { visits, thr }` path, where boot mode
  had set `thr = 1` — compile on second visit, regardless of size or history.

Removing the override leaves those blocks to earn their compile. That is why the compile
count falls by 45 % / 29 % while the baseline install path is untouched.

Not changed, and deliberately: the two `thr = 1` sites in `cool_down` and
`defer_promotion`. They are backoff paths (reached after a wait timeout or a full queue,
once the inline token bucket is spent) and their intent is "do not apply backoff during
warmup", not "compile everything immediately". They were unchanged in the measurement
above, so the win does not depend on them; they are a separate candidate.

### 3.2 The persistent ledger: bypassing "when" with evidence, and the rule that makes it safe

`WIE_JIT_CACHE` (metadata ledger, `jit/cache_persist.rs`) persists per-PE, per-VA
records of blocks that compiled, so a warm boot can skip the "when" decision for
them entirely: a byte-validated hit compiles the block at first sight. That
bypass used to be **unconditional** — any record meant an eager compile — and on a
console-shaped guest that is backwards: of 365 eager compiles, **364 came from the
ledger fast path**, for a guest that ever enters **4** of its 365 compiled blocks.
A warm boot then measured **5× slower** than a cold one (63.6 ms against 12.6 ms,
5 interleaved pairs).

**That 5× figure is historical and must not be quoted as current.** It was taken on
a tree where the eager path took 364 ledger hits; after `0b6e800` (boot mode no
longer forces `thr = 1`, §3.1) plus the UCRT arm plus the rule below, the same
guest is a **one-compile guest** and there is nothing left to remove there.

#### The rule

A record carries an **entry count**, and the eager path is taken only when the
block earned it:

```text
entries == 0            -> never eager   (compiled, never entered)
entries == 1            -> never eager   (entered once; will be entered once again)
entries >= 2            -> eager         (EAGER_MIN_ENTRIES, not a knob)
```

`entries` is **the largest per-thread count, not a total across threads**, and it
is published coarsely (at the first entry, then at each doubling) because the
dispatcher cannot afford a shared increment per entry — a shared map lookup
measured 15-25 ns against ~149 M dispatches per 100 s of DOOM. Two rejected
alternatives, recorded so they are not re-proposed:

- **Sum across threads.** A block each of N threads enters exactly once (a
  barrier, a shared init, a rendezvous) would read `N` and clear the threshold.
  That is the same mistake as promoting every `Ready` record, only wearing
  threads. The compile is also per-VA and happens once: whichever thread arrives
  first compiles it and the rest find it `Ready`, so refusing eager for such a
  block costs **at most one compile on the critical path**, which is the cost the
  rule already accepts for the single-threaded "entered once" case. The maximum
  is therefore the quantity the decision is made against, not a proxy for the sum.
- **One record per thread.** The file would grow with the thread count and ask the
  eager question N times per block, for a compile that happens once. No consumer.

Reachable false negative, accepted: a shared block that no single thread enters
twice is never eager and pays one compile per boot, never one per thread.

#### The evidence

100 s DOOM Retro, `--screenshot`, single runs on a host at load 8-10, ledger
decoded after the run (7 007 records):

| population | records | share | eager? |
|---|---|---|---|
| compiled, never entered | 5 559 | 79 % | no |
| entered exactly once | 496 | 7 % | no |
| entered >= 2 | 893 | 13 % | yes |
| `Never` (failed compile) | 59 | 1 % | no |

**86 % of the records are in the two groups the rule refuses**, i.e. the
population the old unconditional bypass compiled eagerly. The same census on a
4-thread guest (`mt_contention 4 4096`, 103 records) shows cross-thread-once
blocks at 3 %, so the max-vs-sum choice does not move that guest either.

What it buys, measured rather than assumed: **nothing on DOOM in a 45 s or 100 s
window.** 100 s, ledger on vs off: `eager` 282 vs 35, `iced_fb` 69.68 M vs
71.32 M (**-2 %**), `compile_us` 50.63 s vs 51.84 s (-2 %; since `f2e432a` that
field also contains engine-lock queueing, so it is not pure compile work — both
arms used the same meter, so the ratio stands), `bg_compiles` 16 614 vs 16 471
(**+0.9 %**). At 45 s the ledger arm looked 20 % more expensive
in background compiles, and at 100 s that difference is gone. `frames_published`
(684 vs 440) is not usable at a wall-clock kill: the arms stopped at different
guest phases (33.1 G vs 26.5 G instructions).

On the guest the rule was built for it is the whole point: `crt_hello` warm **18.2
ms with the ledger on against 17.4 ms off** (median of 5 interleaved pairs, paired
mean **+2.0 ms**), where the residual is one rewrite of two tiny ledger files and
nothing else. Its ledger holds two records — one "entered once", one "never
entered" — so the rule refuses both and the only eager compile in the run is the
genuine self-loop path, not the ledger.

**Follow-up measured and deliberately not implemented.** Because a probe hit now
falls through to the ordinary promotion machinery instead of compiling eagerly, a
warm run could in principle do *more* promotion work than a cold one. On the 100 s
run it does not (+0.9 % `bg_compiles`, iced 2 % lower), so suppressing the
boot/baseline enqueue for a probe hit stays unimplemented — it would widen the
diff into `pipeline.rs` for no measured effect.

#### Two defects this exposed, and what they cost

- **The ledger was never flushed for a console guest.** `finish_jit_caches()` only
  called `code_cache().finish()`; `Drop for PersistentJitCache` does not run for
  console guests (the last `Arc<JitShared>` outlives the session), and the lazy
  flush needs 256 records or 5 s. `crt_hello` writes **one** record, so it
  persisted nothing for its entire existence while the RUNBOOK claimed otherwise.
  The end-of-session hook now flushes the ledger too.
- **Concurrent flushes dropped the whole ledger.** `flush_now` deliberately drops
  its bookkeeping lock before the I/O, and `write_body`'s temp path was
  per-*process*, not per-call: two flushes wrote the same temp file, the loser's
  `rename` failed `ENOENT`, and the error path reports that as "persistence
  disabled for this run" — taking every record with it. It fired on DOOM Retro's
  exit, i.e. exactly the long run the ledger exists to accelerate. A separate
  `flush_io` mutex now serialises snapshot+write; `concurrent_flushes_all_reach_the_file`
  fails 3/3 with it removed.
- Exit cost, because persisting at all is not free: four rewrites (finish + `Drop`,
  both files, both fsynced) measured **+13.9 ms** of exit latency against a 12 ms
  boot; a `dirty` gate (a flush with nothing recorded is a no-op) and dropping
  `force_fsync` from the end-of-session flush bring it to **+2.0 ms**. One 62-byte
  ledger rewrite costs 2.5-13 ms on this host, so the fsync is not the dominant
  term — the `dirty` gate is.

#### Format, for whoever reads the file

The ledger is bincode (`bincode::config::standard()`, varint integers) behind the
`WIEJITC\x01` magic: `{version, format_version, key, opt_level, entries[]}` with
one record `{va, guest_end, bytes_hash, insn_count, inv_gen, never, compiled_at_opt,
entries}`. `FORMAT_VERSION` is **4**; `entries` was appended last, so a v3 file is
one field shorter and is **migrated, not deleted**: every record decodes with
`entries = 0`, which is exactly "no entry evidence" and therefore never eager. The
file is left in place and rewritten as v4 by the next flush, so a run that records
nothing new cannot lose the previous run's knowledge. A reader that misparsed a v3
record into a plausible count would silently restore the eager-compile storm, which
is why `v3_file_is_migrated_with_no_entry_evidence` asserts the migrated count is
0 and `wants_eager()` is false.

---

## 4. What is observable

`WIE_RUNTIME_PROFILE=1` emits:

```text
jit_compile_cost: insns_us/count 1:41/12 2:88/9 4:... 128:.../...
```

Each pair is `total µs / sample count` for one instruction-count bucket
(`[2^i, 2^(i+1))`), i.e. the raw data behind the table in §3; mean µs per
compile is `us/count`. A *falling* `us/count` with rising `insns` would mean the
fixed cost dominates and the `1/n` shape is roughly right; a rising one means it
is not.

Two caveats, both real: the histogram is per-engine and is one of
`SHARED_DERIVED_STATS`, so a multithreaded guest reports one engine's share;
and background-worker compiles are folded in, so the numbers cover both compile
paths.

## 5. Deliberately not implemented

- **Absolute break-even on nanoseconds** — see §3, items 2 and 3.
- **The corrected threshold shape** — measured and refuted (§3.1, experiment 1):
  flat at 225 / 1 000 / 3 000 is indistinguishable from `900/n` on `x`, and boots
  identical on `h`. Do not re-propose it without a class where the compile count
  actually responds to the threshold.
- **Counter decay** — no evidence yet that any real guest changes phase in a
  way this would help. Implementing it without that measurement would be a
  plausible-looking non-fix; `performance-ceiling.md` §11 is where the compile
  class is tracked and should carry the evidence first.
- **Tier 0 (no-regalloc emitter)** — the top unbuilt lever (ceiling ≥1.4×,
  plausibly ~2× on `7za x`/`h`), and the prerequisite for doing decision 3
  correctly. Constraints recorded in `performance-ceiling.md` §11.5. It is also
  where the *critical-path* compile time is: see §5.1 for the measurement that
  makes it the next slice rather than a nice-to-have.
- **Ahead-of-time region compilation at PE load** — modelled and refuted before
  any code was written; the four numbers are in §5.1. Do not re-propose it
  without a filter that predicts the executed loop set to within ~2×.

  The honest counterweight, and the strongest number anyone has for the region
  path: **`WIE_JIT_REGION=1` cuts `compile_us` 5–8× on a bounded DOOM Retro run
  — 64.4 s and 95.2 s with regions off against 11.5 s and 12.1 s with them on**
  (two 120 s reps per arm, `--max-api 400000000`, `WIE_RUNTIME_PROFILE=1`; 79
  regions formed without `WIE_JIT_REGION_SELF_LOOP`, 244 with it). **Provenance:
  a shared host at loadavg 18–27, so quote the ratio rather than the seconds, and
  read it as a *compile-time* claim — not a frame claim.** The frame effect of
  the region knob is still unestablished; this is what regions are *for* until a
  quiet-host frame A/B says otherwise.

  Census of the same entry arm, taken while scoping the tier-0 emitter, because
  it bounds that work too (4 225 blocks reaching the synchronous compile, one
  7.6 s run): the dominant blockers are **terminator shapes** — `term:call`
  1 013, `term:ret` 556, `term:calldyn` 195, i.e. **42 %** of the entry arm — then
  `Lea` 222, `Push` 158, `Movzx`/`Movsxd`/`Movsx` 121, `Shr` 44, `Movups` 20. **A
  single-pass emitter therefore cannot cover more than ~58 % of the entry arm
  unless it takes call terminators**, which is the fact that decides its shape.
  The first version of that census reported `alu_only` = 8 of 4 225 and was
  wrong: the classifier scanned the terminator as if it were a body instruction,
  so **1 748 blocks (41 %) were rejected by their own `jcc`**. Corrected on the
  same sample: **`alu_only` 1 083 (25.6 %), `with_mem` 690 (16.3 %)** — 42 % of the
  entry arm already expressible, and the memory subset is the same order of
  magnitude as the ALU one. The bug is recorded in
  [`tier0-emitter.md`](tier0-emitter.md) §2.1 so nobody quotes the 0.2 %.
- **OSR (on-stack replacement)** — a loop entered once runs forever without
  touching a visit counter, which is exactly the load-heavy shape this
  emulator must serve. The `is_loop` threshold fakes the *promotion* half but
  not the *transfer*: correct OSR needs an entry point at the loop header plus
  interpreter-frame reconstruction at that header, and it is exactness-critical
  in the same way chaining is. Not started.
- **Benefit-ordered compile queue** — HotSpot orders its compile queue by
  hotness so a bounded queue drops the least valuable job. wie's two-lane
  queue is FIFO within a lane and applies backpressure to the *newcomer*
  (defer promotion, doubling its threshold). An ordering by predicted payoff
  would be better and is a self-contained change to `BgPool`.

### 5.1 AOT region compilation at PE load: modelled, then dropped

The idea is attractive and wrong, so it is recorded rather than rediscovered: at
PE load every byte of the image is known, so loop regions could be formed and
compiled before the first hop instead of waiting for a latch to reveal them.
Four numbers kill it, and only one of them is a timing.

**1. The static loop set is ~30x the executed set.** DOOM Retro's images hold
2.79 MB of `.text` (`doomretro.exe` 1 396 224 B, `SDL2.dll` 1 071 616 B,
`SDL2_mixer.dll` 317 952 B, from their PE section headers). At ~65 B per decoded
block that is ~43 000 static blocks; loops are roughly one block in five, so
**L ≈ 8 000 candidates against E = 244 regions actually formed** in a 100 s run.

**2. It costs ~10 s before the first guest instruction to buy ~0.38 s.** AOT is
paid with the guest *not running*, so `L · c / w` is pure added latency rather
than the overlapped work compile normally is: 8 000 x ~5 ms / 4 workers ≈ 10 s,
against a benefit of `(E/L) · T` = (244/8 000) x 12.6 s ≈ 0.38 s. **~26x
against.** Break-even needs `L` within ~2x of `E`, i.e. a predictor of "which
loops run" that does not exist — the only oracle is running the guest, which is
what the latch fold already uses and why it works.

**3. "AOT got there first" has a 1.5-1.7 % ceiling.** Of a 120 s DOOM run, the
guest-visible share of *all* background compilation is `bg_stalls` 1 242-1 426
totalling **1.83-2.07 s**. That is the entire prize for arriving earlier.

**4. There are no repersist cycles to save.** `WIE_JIT_CODE_CACHE` is **off by
default for a correctness reason** (restoring persisted code replays host
instructions captured by a previous process, so a bug in it executes the wrong
thing rather than running slowly). In the shipping configuration the
invalidation/repersist axis is zero by construction.

The decomposition that reframes this is in §5.1's neighbours above: **64 % of
compiles are already off the critical path** (9-10 k background compiles, 64-95 s
of CPU, fully overlapped). The compile time that is *on* it is the eager arm —
**4 854-4 930 blocks compiled synchronously on the guest thread**, which is a
tier-0 question (§5), not an AOT one. Its cost depends on what else is on the
host and that spread is large: **1.23 ms per compile** measured on a short run
(`compile_us` 9.69 s over 7 854 compiles, guest exited after 7.6 s), against
**4.6-6.8 ms** in the 120 s runs above where four workers compete with a guest
retiring 18 G instructions. So the same 4 930 blocks are **6-25 s** of
guest-thread serial time, not one number — an earlier draft of this section
quoted 12.6 s from a 2.6 ms/block assumption and that was too narrow. AOT's
formation-only variant is not free either: `region.rs` requires formation to run
with the guest thread's `HookWindow` in hand so a region never covers an address
the guest asked to stop at, and at PE load the hook set is not yet known, so a
static region needs a hook-set generation in its key and invalidation when a hook
lands inside its span — new correctness surface on the one path where a mistake
is silent wrong code, for 0.3-0.6 s.

**Host caveat, because the seconds are soft:** these runs were taken on a shared
host at loadavg 18 (a game at 158 %, `XprotectService` at 147 %), where
per-compile CPU measured 4.6-6.8 ms against the 2-4 ms recorded from quiet hosts.
**The counts are the load-bearing part; the seconds are inflated ~2x and the
ratios are what should be quoted.** The 12.6 s figure is the quiet-host
derivation (4 854 x 2.6 ms), not a wall-clock observation, and is labelled as
such wherever it is used.

## 6. How to change this policy safely

Any change here must be validated on `scripts/real-guest-perf.sh` with
interleaved paired arms (median of per-pair ratios, never a single timing), on
a load-gated host, with archive CRC compared between arms. A policy change that
alters which blocks compile is not a no-op even when it produces identical
output — `compiles`, `iced_insns` and `bg_compiles` must be reported alongside
wall time, because they are how you tell "fewer, better compiles" from
"something else changed".
