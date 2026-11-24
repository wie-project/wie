# Compile-selection policy: what to compile, when, and at which tier

Status: **one win shipped, one candidate refuted.** Boot mode no longer lowers the
promotion threshold (§3.1, 1.23x on `h` / 1.12x on `x`). The threshold *shape*
correction was measured and refuted (§3.1). Decision 3 is still blocked on machinery
that does not exist yet.

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
  correctly. Constraints recorded in `performance-ceiling.md` §11.5.
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

## 6. How to change this policy safely

Any change here must be validated on `scripts/real-guest-perf.sh` with
interleaved paired arms (median of per-pair ratios, never a single timing), on
a load-gated host, with archive CRC compared between arms. A policy change that
alters which blocks compile is not a no-op even when it produces identical
output — `compiles`, `iced_insns` and `bg_compiles` must be reported alongside
wall time, because they are how you tell "fewer, better compiles" from
"something else changed".
