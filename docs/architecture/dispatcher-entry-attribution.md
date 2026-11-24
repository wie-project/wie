# The dispatcher entry, after L1 — what is left and what it costs

Date: 2026-11-24 (three rounds on the same day as
[`l1-persistent-frame.md`](l1-persistent-frame.md)). Commits: `94621e2` carries
L1, `8b75698` carries the `RwLock` removal and this document's first two
sections, and the closing sections (§5, §8) are written against `8b75698` with no
further code change. This document answers, in order: *does the per-entry
`RwLock` read pay to remove*; *what remains of the entry path, attributed piece
by piece*; *is the SMC drain mutex the next lever* (no — §5, with the bound and
the design recorded); and *where the next 5% actually is* (§8).

**Result.** The lock read is gone — it was worth **3.47 ns per dispatcher entry**
(measured, 12 pairs, 11/12 favouring) = **~0.6% of `7za a -mx9`**. That is below
this host's per-pair resolution, which is why the end-to-end A/B cannot see it
and the entry-level measurement is the one that decides. Attribution of what is
left: the whole marshalling surface is **18.3 ns of an 85 ns entry (21.5%)**,
of which the largest single item — the 256 B XMM bank copy, 8.2 ns — **cannot be
made conditional without an invariant this project has already been burned by**,
and the second largest (diagnostics, 8.1 ns) is a policy decision, not a
mechanism. Everything else in the entry is block work or the control transfer.

> **Correction (round 3, cross-document): every "projected onto `mx9`" figure below is
> 2.75x too high, because it scaled by ≈220 M dispatcher entries per run.** Re-measured
> at `8b75698` with `scripts/real-guest-perf.sh`, `7za a -mx9` retires **7.623 G guest
> instructions and reports 68.7 / 82.1 / 79.9 M entries** across three reps — entries are
> not a stable counter (±10% for identical instructions; see
> `performance-ceiling.md` §11.6, which resolves the flag on the number itself). The
> **per-entry ns measurements in this document are unaffected** — they were taken at
> identical entry counts on both arms — only the *scaling* to a whole run is. Corrected:
> the `RwLock` read is **0.28 s (0.34%)**, not 0.76 s (0.6%); the full marshalling
> surface is **1.46 s (1.8%)** of an 81.9 s run, not 4.0 s (3.1%); and in §2's table the
> seconds/shares become 0.66 s / 0.80% (XMM), 0.27 s / 0.33% (statistics), 0.38 s / 0.46%
> (per-run resets), 0.28 s / 0.34% (lock read). None of the decisions this document makes
> changes sign under the correction — they were all made against the *entry-level* ratios,
> which is the right unit — but a reader quoting "3.1% of a run" should quote 1.8%.

---

## 1. The lever: one `RwLock` read per entry, for an address that cannot change

`run_compiled` opened with

```rust
let mem_guard = read_unpoisoned(&self.shared.mem);   // std::sync::RwLock
let mem_ptr = (&raw const *mem_guard).cast_mut();
… build the frame …
drop(mem_guard);   // compiled block runs on TLB/pins
```

The guard existed only to derive the pointer. It was dropped *before* the call,
so it never provided mutual exclusion for anything the block did; the block has
always run with `mem` reached through `JitCtx` and no lock held. Meanwhile
`RwLock::read` on this host costs 11.4 ns in isolation (`rustc -O`, 50 M
iterations, uncontended) and the dispatcher runs it ~200 M times per `7za a
-mx9` run.

**What changed.** The pointer is captured once per engine, at construction, and
stamped into the frame per entry exactly like the other raw pointers:

* `PerThreadJitState::new(&shared)` takes **one** read guard, derives
  `*mut GuestMemory`, and stores it as `guest_mem` (and seeds `ctx.mem`).
* `run_compiled` reads `self.thread.guest_mem` — one load, no lock.
* Every *field* read still takes the guard at its own site (`generation`,
  `jit_region_pins`, and all the write paths, which are unchanged).

**Why the address cannot change** (stated on the field, not assumed): the
payload is a field of `RwLock<GuestMemory>` inside `JitShared`, the engine holds
that `JitShared` by `Arc` for its whole life, `Arc` allocations never move, and
no code replaces the payload — every writer takes the guard and calls a method
on it. `RwLock::data_ptr` would have made that explicit but is still unstable
(`rwlock_data_ptr`), so the capture goes through a guard that is then dropped,
which is exactly what the old per-entry code did.

**The invariant from L1 still holds and is now asserted:** the same pointer is
visible from a chain edge as from the dispatcher, because it is a field of the
one frame both paths are handed. `frame_entry_tests` now compares `ctx.mem`
across the chained and dispatched arms *and* against the engine's canonical
`guest_mem`, so a stale or per-path pointer fails loudly:

```
arm at 0x10b00020: the frame must carry the engine's canonical guest-memory
pointer (0x…), not 0x…
```

### Why the end-to-end A/B on `mx9` cannot confirm it, and what was measured instead

Four interleaved pairs, `base` (commit `94621e2`) vs the change, on the 12 MB
`mx9` corpus:

| pair | base wall/cpu | nolock wall/cpu | wall | cpu | entries (M) |
|---|---|---|---|---|---|
| 1 | 124.4 / 129.6 | 122.4 / 128.0 | 1.0164 | 1.0122 | 252 / 189 |
| 2 | 129.2 / 135.3 | 134.7 / 141.0 | 0.9596 | 0.9592 | 198 / 237 |
| 3 | 138.6 / 145.0 | 130.2 / 136.2 | 1.0646 | 1.0649 | 252 / 248 |
| 4 | 132.4 / 138.9 | 134.6 / 142.3 | 0.9844 | 0.9762 | 189 / 267 |
| **median** | | | **1.0004** | **0.9942** | |

The spread is ±6%, and the reason is visible in the last column: **background
compile timing changes the dispatcher entry count by ±35% between arms**
(189 M–267 M), i.e. the two arms do materially different amounts of dispatcher
work. A 0.6% effect cannot be resolved through that.

So the effect was measured where it is not diluted: an **entry-dominated guest**.
`micro-exes/out/chain_hops.exe` with `WIE_JIT_CHAIN=0` retires 6.1 guest insns
per dispatcher entry (vs 48 on `mx9`), so one entry is ~85 ns of a 270 ms run and
the entry count is *identical* in both arms (3 199 997) with identical insns
(19 600 015). Twelve interleaved pairs:

```
base   median emu 278.99 ms   = 87.18 ns/entry
nolock median emu 267.88 ms   = 83.71 ns/entry     delta 3.47 ns/entry
ratios 1.0328 1.0721 1.0492 1.0358 1.0344 1.0316 1.0447 1.0234 1.0289 0.9659 1.0480 1.0405
MEDIAN RATIO 1.0351   11/12 favour the change
```

So: **3.47 ns/entry**, not the 11 ns the primitive costs in isolation — the
acquisition overlaps with the rest of the entry when interleaved with real work.
Projected onto `mx9` (≈220 M entries per run, ~130 s): **0.76 s ≈ 0.6%**, which
is consistent with the four-pair median above (1.000/0.994) and inside its noise.
Kept: it is a strict removal of work from the hottest path, the pointer stability
is provable and documented, and it also removes a read-modify-write on a lock
word **shared by every engine thread** from every entry (unmeasured here; it is
the kind of thing L9 would care about).

## 2. Attribution: what a dispatcher entry still costs

Same instrument, one binary, arms differing only by a temporary ablation mask
(`WIE_L2_ABLATE`, deleted before shipping): 8 reps per arm, 6 arms, interleaved;
every arm has identical entries and insns, so an arm difference *is* the cost of
the skipped piece.

| arm | skipped | emu_ms | ns/entry | Δ vs baseline | share of entry |
|---|---|---|---|---|---|
| 0 | — | 271.48 | 84.84 | — | 100% |
| 1 | **XMM bank snapshot** (16-slot copy-in) | 245.37 | 76.68 | **+8.16 ns** | 9.6% |
| 2 | pin-shape diagnostics | 281.80 | 88.06 | −3.23 ns | 0 (noise) |
| 4 | statistics folding (~17 counter updates + 2 stats writes) | 260.50 | 81.40 | **+3.43 ns** | 4.0% |
| 8 | per-run counter resets incl. `mem_path` zeroing | 256.42 | 80.13 | **+4.71 ns** | 5.6% |
| 15 | all four together | 213.00 | 66.56 | **+18.27 ns** | 21.5% |

The pieces are roughly additive (8.16 + 3.43 + 4.71 = 16.3 vs 18.27 measured
together; the difference is code-layout and the −3.2 ns pin arm believing it is
faster without its own block). Symptom-free cross-check: the pin diagnostics
were already measured at noise level on their own in the previous round (+1.0%
median, reverted), and here they again come out at zero.

Projected onto `mx9` (×220 M entries, ~130 s):

| piece | ns/entry | seconds | share of run |
|---|---|---|---|
| XMM bank snapshot | 8.2 | 1.8 | 1.4% |
| statistics folding | 3.4 | 0.75 | 0.6% |
| per-run resets (`mem_path` dominates) | 4.7 | 1.05 | 0.8% |
| **all marshalling measured above** | 18.3 | 4.0 | **3.1%** |
| lock read (removed this round) | 3.5 | 0.76 | 0.6% |

## 3. Ceiling check: can the XMM snapshot be made conditional? No.

`CompiledBlock` already carries `xmm_may_def_mask` / `xmm_live_mask`, so a
per-block answer is *available*. It is nevertheless unsound at the entry, for a
reason this codebase has already paid for:

> A chained successor may load any live `xmm` from `JitCtx` even when THIS block
> is pure-GPR — the successor is chosen at runtime (chain table, edge IC, RET
> prediction), so the entry block's mask says nothing about what the rest of the
> native run will read.
> — the comment at the snapshot, which records the notepad CRLF bug (`movd
> [rcx], xmm0` writing `0x0000` pairs because a `uses_sse`-gated mask zeroed the
> frame for a GPR block that then chained into an SSE block).

A sound mask would need the union over the *dynamic* chain, which is not
statically known. The only sound alternative is to stop re-publishing the bank
at all: make the frame authoritative and add a coherency flag that every
non-JIT writer of `RegFile.xmm` must clear (the iced fallback in `step_one`,
`restore_thread_context`, host handlers). That is a cross-module invariant whose
failure mode is a silent wrong answer — precisely the class of bug the two
historical cases above belong to — bought for 1.4% of one workload. **Declined**,
with that number.

Two *sound* rewrites were tried and one was kept:

* **Bank accessors** (`gpr_bank()`/`xmm_bank()` returning `&[u64; 16]` /
  `&[u128; 16]`, replacing sixteen bounds-checked `get().copied().unwrap_or(0)`
  calls per bank). Measured **+0.48 ns/entry (median ratio 1.0063, 6/12)** —
  indistinguishable from noise, because the cost was never the bounds check: it
  is the 256 B of load/store traffic itself. **Reverted with that number**; no
  point carrying two accessors for half a nanosecond.
* **The lock removal of §1** — kept, 3.47 ns.

## 4. What is left, from a re-sample of the final binary

30 s of 1 ms samples on `mx9` (20 970 main-thread samples), leaf-attributed:

| frame | share of guest thread |
|---|---|
| generated code, running (incl. chained→chained) | **70.5%** |
| generated code entered from `finish_compiled` | 5.7% |
| `finish_compiled` self (marshalling, folds, writeback) | 4.3% |
| `wie_jit_chain_lookup` (called from emitted code, per hop) | 1.7% |
| `run_until_stop` self (dispatch decision, cache lookup) | 1.2% |
| `__psynch_mutexwait` ← `drain_pending_code_writes` | 0.9% |
| `_platform_memmove` ← `wie_jit_load`/`store` (guest mem helpers) | 1.1% |
| SipHash (per-dispatch cache key) | 0.4% |

So the entry path — the part this and the previous round have been grinding — is
now **≈5–6% of the guest thread**, down from the 32% measured before L1. Of the
4.3% that is `finish_compiled` self, §2 accounts for ~3.1 points, of which
~1.4 is the XMM bank (declined) and ~1.4 is diagnostics/resets (a policy call,
below). What remains after that is the writeback, the `fault`/dirty protocol and
the indirect call — that is, the control transfer, which is **structural**:
`sample` cannot even separate it, because it is one `blr` whose cost is inside
the callee's samples.

**Named, measured, not done in this round** (in the order I would take them):

1. ~~**`drain_pending_code_writes` takes a `std::sync::Mutex` per entry** (0.9% of
   the guest thread visibly in `__psynch_mutexwait`)~~ — **done, §5, and the
   premise was wrong**: that 0.9% is the *compile* lock, not the drain. The drain
   is worth 0.4–1.5%, the generation-gated fast path is designed and measured in
   §5, and it stays unlanded on that number.
2. **Diagnostics gating** (`statistics folding` 0.6% + `mem_path` zeroing ~0.6%):
   both exist only for `WIE_RUNTIME_PROFILE`. Gating them on a "collect stats"
   flag flips 1.2% of the run off for every production run — but it changes
   reported numbers, which is a decision about the project's measurement
   contract, not a mechanism. The previous round already declined to touch
   reported diagnostics for one entry's worth of speed; this is the same trade,
   larger.
3. **`wie_jit_chain_lookup` (1.7%)** is per *hop*, not per entry, and it is
   emitted-code-called: that is L7 (fewer transfers) territory, not the entry.

## 5. The SMC drain mutex: what it guards, how often it fires, and why it stays

The previous round put this at the top of the list on the strength of one line in
its own profile — 0.9% of the guest thread in `__psynch_mutexwait`, read as
`drain_pending_code_writes`. Three of the four things needed to decide it were
measurements nobody had taken. They are taken now, and the attribution was wrong.

### 5.1 What it guards

Compiled code cannot see a host-side store into guest memory, so the write path
publishes the affected guest **page keys** into
`JitShared::pending_code_writes: Mutex<Vec<u64>>` and the dispatcher polls them.
`CpuEngine::mem_write` → `note_code_write` is the only producer (guest stores
inside a compiled block go straight to the arena and are handled by the block's
own decode-time assumptions, not by this list):

| consumer | when |
|---|---|
| `run_compiled`, at the end | every dispatcher entry, after the native frame returned |
| `step_one`, at the end | every iced step |
| `mem_write` | immediately after publishing its own pages |

A poll takes the mutex, `mem::take`s the vector and calls
`invalidate_code_range(page << 12, 4096)` per page. That early-outs unless the
page overlaps a page some `Ready` block was decoded from (`code_pages`); when it
does overlap it drops those blocks, bumps `invalidate_gen` (the generation every
chain edge and backedge re-checks), tombstones the persistent cache and rebuilds
the chain table. The list is capped at 256 pages: past the cap the producer sets
`pending_code_overflow` and the next poll flushes **the whole code cache**.

### 5.2 How often it fires

Instrumented counters, once each on the 12 MB corpus (counters removed again
before landing; entries are the run's own `cache_hits`):

| workload | polls | polls that found a page | pages | overflow flushes | `code_invs` |
|---|---|---|---|---|---|
| `7za a -mx9` | 239 852 130 | **27 529** (0.011%) | 28 988 | 11 | 11 |
| `chain_hops` + `CHAIN=0` | 3 200 130 | 56 (0.002%) | 57 | 0 | — |

The drain is therefore a **per-entry poll that finds nothing 99.989% of the
time**, and the 11 overflow flushes are the run's *entire* invalidation
activity — `code_invs` matches them exactly. A second `mx9` run on the patched
binary (§5.6) reproduced the same 27 529 / 28 988 / 11, which is the functional
cross-check that the patch changed no invalidation.

### 5.3 The 0.9% was not the drain

`sample` attributes the guest thread's `__psynch_mutexwait` samples by stack. All
**188 of 20 970** (0.90%) in the round-2 sample — and 123 of 21 192 (0.58%) in
this round's — sit under

    run_until_stop → step_one / try_compile_from_kind → compile_from_kind_shared
                  → _pthread_mutex_firstfit_lock_slow → _pthread_mutex_firstfit_lock_wait

i.e. `JitShared::engine: Mutex<Option<JitEngine>>` — the single Cranelift
`JitModule` that the guest thread and the four `wie-jit-bg-*` workers must all
serialize on to compile anything. That is the *compile* lock held by a worker,
not the SMC lock. The drain's own lock never reaches `__psynch_mutexwait` because
it is uncontended: its cost appears as `drain_pending_code_writes` self time plus
the inlined `pthread_mutex_lock` stub, **1.43% inclusive** on this round's `mx9`
sample (304 samples), not as a wait.

### 5.4 The bound, and the decision

Two measurements bracket it:

* **In isolation**: `Mutex::lock → mem::take(empty Vec) → drop` is **7.4–8.4 ns**
  per call (50 M iterations, twice, `rustc -O`).
* **In situ**: the generation-gated fast path of §5.5, built behind a knob and
  A/B'd against the lock on the entry-dominated guest — 8 interleaved pairs,
  `chain_hops` + `CHAIN=0`, identical 3 199 997 entries and 19 600 015 guest
  instructions in both arms, identical drain counters (3 200 130 polls / 56 / 57
  / 0): **89.43 → 86.85 ns per entry, 2.59 ns saved**, median ratio 1.0701, 5/8
  pairs favouring (median-of-arms ratio 1.030).

Transfer to `mx9`: 2.59 ns × 240 M polls ≈ 0.6 s ≈ **0.4%**; the sampling figure
of ~7.5 ns/poll (1.43% inclusive) is the pessimistic end; with no overlap at all
the isolated cost gives 2.0 s ≈ **1.5%**. Every reading is at or under the
"couple of percent" bar this round was given, so the patch is **not landed**: a
change to the invalidation protocol — where a missed write is a wrong answer, not
a slow one — does not buy 0.4–1.4% in the closing round. §5.5 records the design
and its invariant anyway, so nobody has to re-derive either.

### 5.5 The three shapes, bounded

**(a) Inline generation counter** — the mechanism `emit_inv_gen_check` already
uses for chain edges. Implemented, measured, reverted:

* `JitShared` gains `pending_code_gen: AtomicU64`; `note_code_write` bumps it
  **inside** the `pending_code_writes` critical section that publishes the page —
  or the overflow flag, which publishes in place of the list.
* `PerThreadJitState` gains `drained_code_gen: u64`, the last value this engine
  drained.
* A poll compares them with one `Acquire` load and returns on equality; on a
  mismatch it takes the lock, takes the pages, and only **then** reads the counter
  to advance the watermark, still under the lock.

**Invariant: every write that could invalidate a compiled block is observed
before that block runs again.** The fast path preserves it because it can only
skip a poll that has nothing to do:

* A bump the poll *observes* proves the publication was already complete — the
  bump is inside the same critical section as the page — and the poll then takes
  the lock path and drains it.
* A bump the poll *misses* proves that publication was still in flight when the
  poll decided; the producer's bump is now visible to the **next** poll, which
  takes the lock path.
* Advancing the watermark only under the lock, after the take, is what stops a
  poll from recording a value that hides a page it did not take; anything
  published after that point bumps past the recorded value.

There is no interleaving in which a completed write is neither drained by the
poll that follows it nor queued for the next one. (The one case the lock itself
handles better is a producer *blocked* on the mutex while the poll runs: the lock
makes the poll wait and drain it, the counter defers it by one poll. Both are
inside the window where the write is concurrent with guest execution, which no
design here closes — the write's own bytes are still moving.)

What fails if it does not hold: `code_inv_smc_write_drops_ready` (existing — write
a byte of a compiled range, the block must be gone), plus three tests written
with the patch and reverted with it: a *second* write after a completed drain
(the producer forgot its bump), a >256-page write (the overflow flush not
republished), and a poll that must not advance the watermark when nothing
completed. Mutating `note_code_write` to drop its bump makes the first fail;
moving the watermark update before the `mem::take` makes the third fail.

The blunt version of the same question — *does this path matter at all?* — is
answered by the `WIE_L2_SKIP_DRAIN` ablation used to build the counters. Skipping
the drain on `chain_hops` does not make the run faster: it changes the program
(3 199 997 → 800 000 dispatcher entries, 19.6 M → 10.0 M instructions, 268 ms →
2.8 s). A dropped invalidation is not a subtle slowdown, it is a different guest.

**(b) Per-region dirty bit set by the write path** — strictly worse than (a), and
rejected on inspection rather than built: the producer would have to translate a
byte range into code *regions* at write time, where today it appends page keys,
and the region set is exactly what the poll has to test anyway. It moves the same
work to the 27.5 K writes instead of the 240 M polls, which is the only thing to
like about it — and (a) already does that with one atomic and no new structure.

**(c) Rare-path-only lock with an atomic fast path** — this *is* (a). The
degenerate variant, a bare `AtomicBool` "list non-empty" set and cleared under
the same mutex, is also sound, and saves a field; but it is a *shared* flag, so it
re-enters the lock whenever any engine published since the last clear, where the
watermark is per-engine and monotone. Single-engine guests cannot tell them
apart.

**Recorded negative, with the number that kills it:** 2.59 ns/entry in situ,
0.4% of `mx9` as measured, 1.5% absolute worst case. Land (a) only with a
multi-engine invalidation test (the `mt-*` guests) and a ≥8-pair `mx9` A/B, in a
round where that is the change.

### 5.6 A second finding on the same path: the 11 overflow flushes

`mx9` flushes the **entire** code cache 11 times per run, because one host-side
write spanned more than 256 guest pages (a 1 MiB `ReadFile` into a guest buffer is
enough). Each flush discards every compiled block — ~600 blocks × ~60 µs ≈ 36 ms
— so the bill is ~0.4 s ≈ 0.3% of the run, plus chain-table repair.

Filtering non-code pages out at `note_code_write` is **not** sound: a background
worker may be compiling a block from a page that is not yet in `code_pages`, and
`pending_code_write_overlaps` is precisely what catches that at install time. The
honest fix is a publish-then-verify protocol (bump a generation, let in-flight
installs re-check) rather than a filter. At 0.3% that is not a change worth making
now — but it is the one remaining structural wart on this path, and it is where
the next session should look if invalidation ever shows up in a profile.

## 6. Gates

Round 1–2 (the code change, commit `8b75698`):

| gate | result |
|---|---|
| `cargo nextest run --workspace` | **2035/2035 passed**, 0 skipped |
| `cargo clippy --workspace --all-targets` | **0 warnings**; same with `-D warnings` |
| `cargo fmt --all -- --check` | clean (workspace-wide) |
| `scripts/check.sh --only file-sizes --only dep-direction --only winapi-port --only state-bindings` | all pass, `whole-state bindings ok (439 = budget 439)` |
| `scripts/run-micro-suite.sh all --short` | **65 fixtures run, `micro-suite (all): ok`** |
| `mx9` output, all 8 A/B runs | archive **4 423 813 B, SHA-256 `bcbc277e…`** — identical in both arms |
| guest instructions, all 8 A/B runs | identical to 7×10⁻⁷ |

The measured binary is byte-identical to the shipped one
(`shasum target/release/wie` after the final revert equals the A/B binary), so
the numbers describe the code in the tree.

Round 3 (this round — **no code change lands**: §5's patch and its three tests
were built, measured and reverted, and every other result here is a measurement):

| gate | result |
|---|---|
| `cargo nextest run --workspace` | **2035/2035 passed, 0 skipped** (161 s) |
| `cargo clippy --workspace --all-targets` | **0 warnings**; same with `-D warnings` |
| `cargo fmt --all -- --check` | clean (workspace-wide) |
| `scripts/check.sh --only file-sizes --only dep-direction --only winapi-port --only state-bindings` | all pass, `whole-state bindings ok (439 = budget 439)` |
| `scripts/run-micro-suite.sh all --short` | **`micro-suite (all): ok`** (65 fixtures; needs `</dev/null` — a console fixture blocks on stdin otherwise, which is what stalled the first attempt) |
| `WIE_CPU=iced` on a real guest | `7za h D:\rnd\r0.bin` under the interpreter: `CRC32 CE42D530`, `Everything is Ok` |
| `mx9` archives, this round's A/B | size identical (4 423 813 B) and **both arms `0d573dea…`**; see §7 on why that is not the round-2 constant |
| guest instructions, this round's `x` arms | identical to 3×10⁻⁶ (265 073 007 vs 265 073 293) |

## 7. Honest notes

* **The 0.6% end-to-end claim is a projection, not an A/B result.** The A/B
  (§1) is consistent with it but cannot resolve it; the 3.47 ns/entry *is* an
  A/B result, on a guest where the entry dominates. Anyone re-running this should
  measure on an entry-dominated workload, or `WIE_JIT_BG=0` to pin the entry
  count, not on `mx9` alone.
* **The attribution table is measured on `chain_hops`, not on `mx9`.** The ns
  figures describe the entry path's own code, which is identical in both; the
  `mx9` column is that number times its entry count. In `mx9` the frame and
  `RegFile` lines are more likely to be evicted by the block's work, so the
  `mx9` projections are, if anything, low.
* **The `mx9` entry count is not stable across arms** (189 M–267 M, from
  background-compile timing). That is a property of the workload, not of this
  change, and it is why the previous round's `1.33x` was reported as a median
  over 12 pairs rather than any single run.
* Not verified: nothing about multi-engine contention from removing the shared
  lock word per entry — it would need the `mt-*` guests and the peer's
  `scripts/mt-scaling.sh`, which is a different experiment.
* **§5's two numbers disagree, and both are in the document on purpose.** The
  in-situ A/B of the fast path says 2.59 ns/entry; `sample` says the whole drain
  is 1.43% inclusive (~7.5 ns/poll). The likely reason is that the A/B arms both
  carried the counters, and a `fetch_add` on a hot cache line overlaps with the
  mutex's own locked operation, so the A/B *understates* what removing the lock
  would save. An honest re-measurement is the landed patch against `8b75698` on
  `mx9`, ≥8 pairs, with no instrumentation in either arm — that experiment is the
  one §5's negative is conditional on.
* **The `mx9` archive constant moved, and it is not a codegen change.** The
  archive stores each corpus file's timestamp: `bcbc277e…` in rounds 1–2,
  `0d573dea…` in every run of this round, at the same 4 423 813 bytes, from
  `94621e2`-era and `8b75698` binaries alike. The corpus files' host mtimes moved
  during runs this session (`stat` on `/tmp/corpus_l1/rnd/r0.bin` before/after a
  run), so the bytes differ while the *content* does not. The gate that survives
  this is arm-versus-arm identity within a session, which held exactly; the
  historical constant should be re-established on a quiet host before anyone
  treats a mismatch as a regression.
* **This round was measured on a shared machine.** A peer was building and
  benchmarking in a separate worktree throughout: `mx9` moved between 124 s and
  197 s for the same binary, and the first `chain_hops` pairs swung 0.75–1.27.
  Every ratio above is a median of interleaved pairs, and the two `x` pairs that
  carry the 1.67x claim are the two that ran on the quietest window (12 087/7 232
  and 12 088/7 230 ms — a 0.01 s spread between pairs, which is why they are
  quoted individually).

## 8. Closing: the ranking after the entry work, and where the next 5% is

Re-sampled for this section rather than carried over: a symbolized build of the
same commit (`CARGO_PROFILE_RELEASE_STRIP=false … DEBUG=1`), 30 s of 1 ms samples
on `mx9`, and merged samples of four `x` runs (the extraction class, 265 M guest
instructions per run). `mx9` has 21 192 main-thread samples, `x` 35 978.

| frame | `mx9` | `x` |
|---|---|---|
| generated code, running (incl. chained→chained) | **65.1%** | 5.7% |
| generated code entered from `finish_compiled` | 8.2% | 15.7% |
| `finish_compiled` self (writeback, folds, resets) | **6.2%** | 3.9% |
| Cranelift compile on the guest thread (`compile_block`, regalloc, machinst, …) | ~1.5% | **~20%** |
| `__psynch_mutexwait` ← compile lock (`compile_from_kind_shared`) | 0.6% | **34.5%** |
| `wie_jit_chain_lookup` (called from emitted code, per hop) | **2.5%** | 1.2% |
| `CpuEngine::run_until_stop` self (dispatch + cache lookup) | 2.3% | 0.8% |
| `drain_pending_code_writes` (incl. its inlined `pthread_mutex_lock`) | 1.4% | 0.3% |
| SipHash over the block-cache key | 1.1% | 0.8% |
| `_platform_memmove` in `wie_jit_load`/`store` | 1.1% | 0.2% |
| `tlb_page_ptr` under the mem helpers | 0.9% | — |
| `semaphore_timedwait_trap` (guest thread parked on a bg compile) | 1.0% | 1.4% |

**The dispatcher is no longer where the time is.** Entry-path frames are ~8.5% of
`mx9` (`finish_compiled` 6.2 + `run_until_stop` 2.3) and under 5% of `x`, against
32% before L1. The 5–6% figure of §4 counts only the marshalling share of that;
the difference is where the boundary is drawn (this build also inlines more of
`finish_compiled`'s callees than the round-2 binary did — 6.2% self here against
4.3% there, same source), not a regression. `run_until_stop`'s 2.3% contains the
block-cache lookup, of which SipHash is 1.1 points. Everything left in it is
either writeback (per-run, structural), the control transfer (a `blr`, invisible
to `sample` because its cost is the callee's), or already bounded and declined in
§2 and §5. No 5% is available there.

Where the next 5% *is*, in the order I would take it:

1. **Compile serialisation on the guest thread — 34.5% of `x`, 0.6% of `mx9`.**
   The single `JitEngine` mutex is held by a worker 1/3 of the time on a
   compile-heavy run, and the guest thread blocks on it inside `step_one`. Two
   facts say this is not just contention but surplus work: `WIE_JIT_BG=0` makes
   `x` **1.67x faster** (median of 4 interleaved pairs: 12 087/7 232 and
   12 088/7 230 ms, the noisy pair 18 421/9 462; identical 265 M guest
   instructions), and the reason is volume: for the same 265 M guest
   instructions the default arm issued **5 868 compilations where the inline arm
   needed 425** (`bg_compiles=5828` + `compiles=40` vs `compiles=425`), every one
   of them serialized through that mutex, and only 0.32 s of the guest's wait was
   `bg_stall_us` — the rest is contention with compiles it never asked for. The
   lever is therefore *not* "more workers" or "fewer workers" (that sign is
   already measured: the pool is worth 1.7–1.8x on 8-thread `-mmt=8`,
   `performance-ceiling.md` §10.5) but **speculative-compile volume and lock hold
   time**: stop prefetching blocks the guest never enters, and stop making the
   guest thread wait for a lock a worker holds for a whole Cranelift compile.
   This is L9 territory and it is now the biggest single item in the profile of a
   real workload. (Not mine to land here: it changes the promotion policy for
   every workload, and the multi-threaded sign is the opposite one.) **Part of it
   is already claimed as this document closes:** a peer's in-flight change skips
   the background tier-1 enqueue for blocks that cannot tier up — which was a
   duplicate compile of the RIP installed two lines later — and measures
   **1.57x on `7za h`, 1.62x on `7za x`** with `bg_compiles` 2139 → 336 and
   5951 → 668, in the same direction and for the same reason as `WIE_JIT_BG=0`
   above. What that leaves on this lead is the *general* case: speculative
   prefetches the guest never enters, and the lock hold time of the compiles that
   remain.
2. **`wie_jit_chain_lookup` — 2.5% of `mx9`, called once per hop** (1.9 G hops).
   It is a helper call emitted into every chain edge; the probe itself is a few
   instructions. Inlining the slot probe into emitted code (or caching the last
   hop target in the frame) is the one remaining emitted-code item with a
   plausible ≥1% payoff and no layout change beyond `JitCtx` (which would force
   the `CODE_FORMAT_VERSION` bump §1 of `l1-persistent-frame.md` describes).
3. **SipHash over the block-cache key — 1.1% of `mx9`, 0.8% of `x`.** Per
   dispatch, hashing an 8-byte VA with SipHash13 through `RandomState`. A
   multiply-fold `BuildHasher` for the guest-VA maps is a contained change with a
   measurable 1% attached and no correctness surface beyond hash quality (guest
   VAs are not attacker-controlled in the hostile sense, and the map is
   open-addressed over a u64). It is the cheapest ≥1% on the list.
4. **The remaining `finish_compiled` writeback (~3.4 of the 6.2 points).** The
   two halves that are removable were already bounded and declined: the XMM bank
   snapshot because per-block masks are unsound across a runtime-chosen chain
   successor (§3), and the diagnostics reset because it changes reported numbers
   (§4). What is left is the GPR writeback (already `ALL_DIRTY` on the Cranelift
   path) and the fault/dirty protocol — structural.

**Nothing here is a dispatcher problem any more**, which is the honest summary of
the investigation: 32% → 5-6% of the guest thread, and the profile has moved to
(a) generated code, which is Cranelift's output and the guest's own work, and
(b) compile scheduling, which is a threading question. Both are projects, not
levers, and neither is mine to land in a closing round.
