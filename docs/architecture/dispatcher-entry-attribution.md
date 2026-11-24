# The dispatcher entry, after L1 — what is left and what it costs

Date: 2026-11-24 (second round on the same day as
[`l1-persistent-frame.md`](l1-persistent-frame.md)). Base: commit `94621e2`,
which carries L1. This document answers two questions in order: *does the
per-entry `RwLock` read pay to remove*, and *what remains of the entry path,
attributed piece by piece*.

**Result.** The lock read is gone — it was worth **3.47 ns per dispatcher entry**
(measured, 12 pairs, 11/12 favouring) = **~0.6% of `7za a -mx9`**. That is below
this host's per-pair resolution, which is why the end-to-end A/B cannot see it
and the entry-level measurement is the one that decides. Attribution of what is
left: the whole marshalling surface is **18.3 ns of an 85 ns entry (21.5%)**,
of which the largest single item — the 256 B XMM bank copy, 8.2 ns — **cannot be
made conditional without an invariant this project has already been burned by**,
and the second largest (diagnostics, 8.1 ns) is a policy decision, not a
mechanism. Everything else in the entry is block work or the control transfer.

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

1. **`drain_pending_code_writes` takes a `std::sync::Mutex` per entry** (0.9% of
   the guest thread visibly in `__psynch_mutexwait`, plus its own self time) to
   `mem::take` a usually-empty `Vec` of self-modifying-code pages. An atomic
   length/timestamp in front of the lock would skip it in the common case. It is
   the same species as the lock removed here and bigger — but it is the SMC path,
   where "missed one round" is a wrong answer rather than a slow one, so it wants
   its own round and its own test, not a drive-by.
2. **Diagnostics gating** (`statistics folding` 0.6% + `mem_path` zeroing ~0.6%):
   both exist only for `WIE_RUNTIME_PROFILE`. Gating them on a "collect stats"
   flag flips 1.2% of the run off for every production run — but it changes
   reported numbers, which is a decision about the project's measurement
   contract, not a mechanism. The previous round already declined to touch
   reported diagnostics for one entry's worth of speed; this is the same trade,
   larger.
3. **`wie_jit_chain_lookup` (1.7%)** is per *hop*, not per entry, and it is
   emitted-code-called: that is L7 (fewer transfers) territory, not the entry.

## 5. Gates

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

## 6. Honest notes

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
