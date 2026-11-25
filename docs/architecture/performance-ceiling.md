# The performance ceiling: what to change, in what order

Date: 2026-11-24. Branch `feat/dll-coverage`, base commit `2e0a1dd`. All figures below
were measured in this session on the real guests in `real_exes/`, not on micro fixtures.

This document answers one question: **what must we change or add so that a real
Windows application runs as fast as it can under WIE?** It is a ranked list of
levers with a *measured* current cost, an *upper bound* on what removing it can buy,
and an explicit list of dead ends with the number that kills each one.

A 5% win is not the answer here. The two levers that are worth 1.4x and 1.9x are
both architectural, and both are in the same place: the boundary between the
dispatcher and compiled code.

---

## 0. TL;DR — the ranked table

`Ceiling` is an upper bound on wall-clock gain on the named workload if that lever
were made free and nothing else changed: `1 / (1 - measured share)`.

| # | Lever | Measured current cost (real guest) | Ceiling | Effort | Risk | Reversible | Kind |
|---|---|---|---|---|---|---|---|
| ~~**L1**~~ **LANDED** | Per-dispatch-entry frame marshalling: `JitCtx` is built by value and copied back on every dispatcher entry (4.6 KB of struct copy + ~100 scalar copies) | **32% of the guest thread on `7za a -mx9`** (memmove alone 8.9%); ≈180 ns/entry × 225.9M entries ≈ 40 s of 102.6 s. `x`: 24% | **≤1.45x on mx9/mx1** (1/(1−0.32)) | M (≈400–600 LOC, 2 files) | Medium: ABI + lifetime; must keep chain-entry and dispatcher-entry state identical | Yes (feature flag) | Architecture (`jit/pipeline.rs` + peer-owned `jit/lower/mod.rs`) |
| **L2** | Compile serialisation: one `Mutex<JitEngine>` held across Cranelift's `compile_function` for every block, on every thread | **`x`: 24% of the guest thread blocked at `lock_unpoisoned(&self.engine)` (`shared.rs:1137`) + 25% compiling = 49%**; mx9 startup window: 53% lock-wait; mx9 steady state: 1% | **1.5–1.9x on the extraction class** (1/(1−0.49) = 1.96); ≤1.05x mx9 | L (per-thread `JITModule`s, chain table gains a module dimension) | High: `FuncId` is module-local; cache/chain keys must carry the module | Hard | Architecture |
| **L3** | Cranelift codegen cost per block (88% of compile time is Cranelift's own regalloc+isel, 4% is our CLIF emission) | compile CPU: 20.3 s (`mx9`), 12.7 s (`mx1`), 6.5 s (`x`), 3.2 s (`h`); guest-visible stall 0.5 / 1.6 / 10.8 / **10.6%** | **≤1.12x on `h`** (1/(1−0.106)); ~1.1x on `x` when loaded; 1.005x on mx9 | XL (second backend: no-regalloc baseline emitter + tiering) | High | Yes (tier choice) | Architecture |
| **L4** | First-execution compiles: a block executed **once** is compiled anyway | eager compiles: 7592 (`mx9`), 7173 (`mx1`), 5784 (`x`), 1893 (`h`); `h` compiles 2087 blocks to retire 5.5M instructions | bounded by the stall it removes: **1.11x `h`**, ~1.1x `x`, ≈1.005x mx9 | S–M | Low | Yes | Patch-ish (needs a "compile on 2nd execution" decision, not a knob) |
| **L5** | Emitted-code quality: base tier compiles at Cranelift `opt_level=none` | emitted code = **58% of mx9's guest thread**; `WIE_JIT_OPT=speed` on mx9 (below) | see §4.5 measurement | S (default flip) | Medium: compile cost per block ~1.5–1.7x (already measured in `docs/status.md`) | Yes (knob) | Patch |
| **L6** | Chain-hop machinery | chain edges are already winning: `WIE_JIT_CHAIN=0` costs **1.53x** on `mx1` (median of 2 interleaved pairs) | no gain available by *removing* work; the gain is in making entries cheap (L1) and fewer (L7) | — | — | — | Landed |
| **L7** | Region/trace formation: entries per guest instruction | **81.2M entries on mx9 (93.9 insns/entry), measured 2026-11-25** — chaining + the persistent frame already removed two thirds of the 226M/41.6 this row was costed with; `x` 26.8 insns/entry. `WIE_JIT_SUPER=all` (that mode is **deleted** from the code) does **not** form regions — it is a per-block memory path — and measured 1.008 on mx9 — see §4.7 | multiplies a share L1 already cut: 400-insn regions would divide entries ~10x, but the entry path is **≈5–6% of the guest thread now** (85 ns/entry), so **≈1.05x, not the ≈1.4x this was costed at** — §4.7 | L (region discovery + exit stubs) | High (exactness on faults/SMC) | Yes | Architecture |
| **L8** | Host stops / API resolution / handler dispatch | `handler_ms` ≤ **1.2%** of wall on every measured class (0.11% mx9, 0.22% mx1, 1.18% `x`, 0.50% `h`); `resolve_ms` ≤ 0.54 ms | **≤1.2%** — dead end for load-heavy guests | S | Low | Yes | Patch |
| **L9** | Threading / N guest threads | 4 compile workers are **85–90% idle** while the guest blocks on the compile mutex 24% of the time; the mutex, not CPU, is the serialising resource | bounded by L2; not independently measurable here | L | High | — | Architecture (peer-owned measurement) |

**Dead ends, with the number that kills each:** guest-VA==host-VA translation
(≤1.4%), iced residue (≤0.8%), in-guest stubs for hot exports (≤1.2%), guest-heap
allocation patterns (0.004%), WinAPI name resolution (0.01%), the existing
`WIE_JIT_TARGET_WORK` lazy-compile knob (0.996 median). See §5. Where the
per-instruction cost itself goes — the census behind the "emitted code" rows above
— is §12.

---

## 1. Method, and how to read the numbers

**Binary.** `/tmp/bins/h_sym`: release build of `2e0a1dd` with
`CARGO_PROFILE_RELEASE_STRIP=false CARGO_PROFILE_RELEASE_DEBUG=1` so `/usr/bin/sample`
resolves symbols (the shipped release profile sets `strip = true`, which turns every
frame into `???`). Every A/B in this document is one binary with an env or source
difference — never two independently built trees.

**Host.** Shared machine, 7 users, 1-minute load average **3.3–7.4** across the
session. Every absolute time below is therefore a *spread*; ratios are interleaved
A/B pairs (A,B,A,B,…) reported as the median of per-pair ratios, plus the raw
samples. When a single number appears it is the median of that arm, and the spread is
given when the spread is bigger than the effect.

**Workloads** (calibrated so one run of the expensive mode lands in ~30–100 s):

| Class | Command | Notes |
|---|---|---|
| compute | `7za a -mx9 -y D:\out9.7z D:\rnd D:\txt D:\mid` over `/tmp/corpus` (6.7 MB: 4 MB incompressible random, 2 MB compressible text, 1 MB mid) | LZMA, the CPU-heaviest real mode |
| IO+compress | same with `-mx1` | contrast |
| extraction | `7za x -y -oD:\ex C:\out.7z` | read + I/O path |
| read+hash | `7za h D:\rnd\r0.bin` | least allocation-churny |
| GUI | `notepad.exe C:\big.txt` (1.5 MB, 23k lines) | see §2.1 for what is not drivable |

**A measurement hazard worth recording.** Guest `C:\` is the host bottle
(`~/Library/Application Support/WIE/bottle/drive_c`), so `rm -f /tmp/corpus/out.7z`
does **not** delete the archive a previous run produced: the next `7za a -y` *updates*
an existing archive instead of compressing from scratch, and the instruction count
silently drops by ~4x (measured: `insns=1.92G` on a fresh archive vs `0.507G` on an
**unchanged** one). Every benchmark below writes its archive to `D:` (the host
directory passed as `--drive-d`), where `rm` works. Any future harness that leaves the
archive in the bottle is measuring the wrong thing.

**How the counts are obtained.** `WIE_RUNTIME_PROFILE=1` prints wall/CPU, `emu_ms`,
`handler_ms`, `resolve_ms`, host-stop counts, and the `jit:` line
(`insns`, `iced`, `cache_hits` = dispatcher entries, `bg_stalls`/`bg_stall_us` = time
the *guest thread* waited for a background compile, `load`/`store` = host memory-helper
calls). `/usr/bin/sample <pid> 5` gives the per-thread split. Note that
`WIE_RUNTIME_PROFILE` adds two `Instant::now()` calls **per quantum**, not per
instruction (20,424 quanta on mx9 ⇒ ~2 ms), so the profiling overhead does not distort
the stack attribution.

---

## 2. The workload matrix

> **Round-3 status of this section:** every row below was measured at `2e0a1dd`,
> **before** the persistent-frame change (`94621e2`, `8b75698`). The `mx9`/`x`/`h` rows
> are also not reproducible any more, for a reason §1 documents but that this run got
> wrong: the archives were written to `C:\` (the bottle) and only `rm`'d on the host, so
> at least one of these runs *updated* a pre-existing archive instead of compressing
> from scratch. Re-running the same binary on the same corpus today gives 7.64 G insns /
> 122.6 M entries where the table says 9.37 G / 225.9 M. **Treat this table as a
> historical record; §11.1 has the post-frame-fix matrix measured with
> `scripts/real-guest-perf.sh` (which refuses to measure an archive it could not
> delete).**

Profiled, `/tmp/bins/h_sym`, corpus `/tmp/corpus`. Each row is one full run.

| | `a -mx9` | `a -mx1` | `x` | `h` |
|---|---|---|---|---|
| wall_ms | 102 711 | 36 686 | 9 462 | 2 057 |
| emu_ms | 102 581 (99.9%) | 36 581 (99.8%) | 9 333 (98.8%) | 2 038 (99.5%) |
| handler_ms | 111.6 (0.11%) | 79.1 (0.22%) | 109.8 (1.18%) | 10.3 (0.50%) |
| host_stops / charged | 20 424 / 9 687 | 12 922 / 6 108 | 19 560 / 9 143 | 5 218 / 2 545 |
| guest insns | 9.370 G | 3.162 G | 178.1 M | 5.53 M |
| iced insns | 34.3 M (0.36%) | 28.2 M (0.89%) | 29.7 k | 10.9 k |
| dispatcher entries | **225.86 M** | 32.37 M | 8.97 M | 4 601 |
| guest insns / entry | 41.6 | 97.7 | 19.9 | 1202 |
| chain hops | 1.147 G | 374.6 M | 3.09 M | 11.3 k |
| compile CPU (all threads) | 20.32 s | 12.70 s | 6.50 s | 3.20 s |
| guest waits for compile | 0.495 s (0.5%) | 0.569 s (1.6%) | 0.028 s (0.3%) | **0.218 s (10.6%)** |
| guest insn rate | 91.3 M/s | 86.1 M/s | 19.0 M/s | 2.7 M/s |

`x` re-run while the host was at load 6 (`/tmp/ceil/prof_x_full.log`): wall 7 664 ms,
emu 7 580 ms, 6.92 M entries, **`bg_stall_us=826 604` = 10.8% of wall**. Compile cost
is not a constant: on a loaded machine it stops being hidden behind the workers.

### 2.1 What could not be measured, and why

* **`notepad.exe` with `--gui` is not drivable in this environment.** With
  `WIE_INPUT_SCRIPT` and a 1.5 MB document it blocks in
  `CFUserNotificationDisplayAlert: called from main application thread, will block
  waiting for a response` and never reaches the input script (rc=124 after 120 s).
  That is a host-dialog/harness limitation, not a WIE perf result.
* **`notepad.exe --screenshot`** does run headless (rc=0, one frame, 192 ms wall) but
  captures the frame *before* the document is loaded: it retired 1772 instructions
  total. It is a **startup** measurement, not a load measurement — see §3.5.
* **DOOM Retro is not measurable headlessly here.** `--persistent` (the only mode that
  does not need a window server) rejects guest argv, so the `-iwad` argument it needs
  cannot be passed: it falls into `comdlg32!getOpenFileNameA` (an IWAD picker) and
  exits. `--gui` needs a window server, which this environment does not have. The
  existing `docs/jit-architecture-review.md` figure (startup 364 M instructions in
  ~22 s ⇒ 16.5 M insn/s) remains the only Doom datum and it is a startup number.
* **2048.exe** was not driven: it needs keystrokes and a window; no headless input
  path exists for it (`--input-script` requires `--gui`).

Nothing in the ranking below depends on the GUI/game classes: they are *startup*
workloads (sub-second, handler-dominated), and §3.5 shows why a startup-dominated
guest is a different optimisation problem that does not make the cut.

---

## 3. Where the time actually goes, per class

### 3.1 Compute (`7za a -mx9`) — the guest thread, steady state

`/usr/bin/sample` over a 5 s window in LZMA's steady state (late window, 4208 samples
on the guest thread):

| Bucket | Share of guest thread |
|---|---|
| emitted code (`???`, no symbol — Cranelift output) | 58% |
| **dispatcher (`finish_compiled`/`run_compiled` + engine loop)** | **32%** |
| host memory helpers (`wie_jit_load`/`store` misses) | 7% |
| compile-lock wait | 1% |

Within the 32% dispatch bucket, the largest single leaf is `_platform_memmove`:
**373 of the guest thread's 4208 samples (8.9%)**, and its caller chain is
`run_until_stop → CpuEngine::run_until_stop → JitCpu::finish_compiled → memmove`
(`run_compiled` is inlined into `finish_compiled`). The rest is
`finish_compiled` self (5.8%), `run_until_stop` self (3.3%), `step_one` (1.5%) — the
field-by-field `JitCtx` construction, the ~25 `saturating_add` stat folds, and the
`RegFile` ⇄ `JitCtx` register copies.

**Why there is a memmove at all.** `JitCtx` holds the per-thread JIT state *by value*:
`tlb: GenTlb<u64, TlbValue, 16, 4>`, `pins: [MemPin; 8]`, `shadow_ret: [u64; 32]`,
`gpr: [u64; 16]`, `xmm: [XmmSlot; 16]`, plus ~50 scalars. `run_compiled` copies
`self.thread.tlb` into the frame and copies it back on return. One `GenSet` is
`tags[4]·8 + values[4]·16 + gens[4]·8 + rr + pad = 144 B`, so **the TLB alone is
2304 B, copied in and out = 4.6 KB per dispatcher entry**. At 225.9 M entries that is
**~1.0 TB of struct copy per mx9 run**, which is what the 8.9% memmove share is.

The share is not an artefact of one sample: the early window (first ~8 s) is
`compile-lock-wait 53%, compile 44%` on the same thread — i.e. mx9 has a compile-bound
startup and a dispatch-bound steady state, and the steady state is 95% of the run.

**Independent confirmation from an ablation.** `WIE_JIT_CHAIN=0` forces every block
edge back through the dispatcher. On `mx1s` (same binary, interleaved, identical
instruction counts in both arms — `insns=507 240–242 k`):

```
mx1s,def,    23135.91   (pair 1)      mx1s,nochain,26535.73
mx1s,def,    11996.83   (pair 2)      mx1s,nochain,22984.96
per-pair ratios 1.147, 1.916 — median 1.53
```

Chaining is worth ~1.5x. Turning the same measurement around: the baseline run has
~5.2 M entries and ~60 M hops; with chaining off there are ~65 M entries. One extra
dispatcher entry therefore costs **≈180 ns (~550 cycles)** — the same order as the
32% × 102.6 s / 225.9 M = 145 ns/entry the stack profile implies. Two independent
methods, same answer.

### 3.2 IO + compression (`a -mx1`)

Same shape as 3.1 with more compile: 35% of wall is compile CPU (12.7 s), of which
the guest sees 1.6% as stalls. iced is 0.89% of retired instructions here (the highest
of the set) but that is still ≤0.9% of wall at iced's ~8x per-instruction cost.

### 3.3 Extraction (`7za x`) — **the compile-lock class**

```
   thread (guest)  n=3350   compile=25%  compile-lock-wait=24%  emitted=24%  dispatch=24%
   wie-jit-bg-0..3 n=3350   idle/wait=88-90%  compile=6-7%  compile-lock-wait=4-5%
```

The guest thread spends **811/3350 = 24.2%** of its samples in
`_pthread_mutex_firstfit_lock_wait`, and the parent frame is unambiguous:

```
_RNvMs1_..._JitShared24compile_from_kind_shared  (in h_sym) + 2088  shared.rs:1137
  _pthread_mutex_firstfit_lock_slow  → _pthread_mutex_firstfit_lock_wait   810 samples
```

`shared.rs:1137` is `let mut eng_guard = lock_unpoisoned(&self.engine);` — the one
mutex every compiling thread serialises on, held for the whole `compile_block` call.
The four background workers are simultaneously **88–90% idle**: the guest is not
competing for CPU, it is waiting for a lock held by whichever worker is compiling. The
`bg_stall_us` counter (826 ms = 10.8% on this run) counts the same phenomenon from the
runtime's side.

Meanwhile `handler_ms` is 1.18% and `resolve_ms` 0.54 ms: this class is not I/O-bound
in the emulator, it is **compile-bound**.

### 3.4 Startup / read+hash (`7za h`)

2 087 background compiles and 1 893 eager compiles to retire 5.53 M instructions;
**10.6% of wall is the guest waiting for those compiles**. `h` is a startup workload
wearing a benchmark's clothes: 4 601 entries, 1202 instructions per entry.

### 3.5 GUI (`notepad --screenshot`, 1.5 MB document)

192 ms wall, 118 ms emu, **`handler_ms`=68.5 ms = 36.7% of emu**, 246 host stops for
1772 retired instructions (`stops_per_1k = 126`). For a GUI launch the cost is host
API work, not guest execution — and the absolute number is 0.2 s. This class optimises
*differently* (fewer stops, cheaper handlers) and does not move the needle on a
load-heavy guest. It is the reason L8 appears in the table at all, and the reason its
ceiling is 1.2%.

### 3.6 The one thing all classes share

Whatever the class, the emulator's own overhead is concentrated in exactly two places:
**the dispatcher entry** (L1, L7) and **the compile path** (L2, L3, L4). Emitted-code
quality (L5) is the third, and it is the only one that touches the 58% "emitted" share
on the compute class.

---

## 4. The levers

### 4.1 L1 — Stop rebuilding the frame on every dispatcher entry (≈1.45x on mx9/mx1)

**Current cost.** 32% of the guest thread on mx9, 24% on `x`; ≈145–180 ns per entry,
over 225.9 M entries on mx9, 32.4 M on `mx1` and 9.0 M on `x`.

**What is actually necessary.** Reading `run_compiled` (`crates/wie-cpu/src/jit/pipeline.rs`),
per entry the code:

1. refreshes pins when `mem_gen` changed (cheap, gated),
2. recomputes pin/gen *diagnostic* stats (8 pins, every entry, though it only changes
   when pins change),
3. copies `tlb` (2304 B), `pins`, `shadow_ret`, `gpr[16]`, `xmm[16]` into a stack
   `JitCtx`,
4. initialises ~50 more fields, 25 of them straight copies of `self.thread.*`,
5. calls the block, then
6. folds ~40 statistics counters,
7. copies ~25 fields back out, and writes `gpr[16]`, `xmm[16]`, `rflags`, `rip` into
   `RegFile`.

Steps 3 and 7 are 4.6 KB + 512 B of memory traffic each way. Steps 4/7 for
`tlb`/`pins`/`sticky`/`edge_ic`/`shadow`/`heap`/`chain_slots`/`large_free`/`inv_gen_ptr`
exist only because the frame is a **temporary**: the successor block reads and writes
the TLB and pins *through the frame*, so a fresh frame every entry forces a copy in
and a copy out.

**Patch shape (small enough to land, big enough to matter).**

* Own the frame per thread instead of per call: `PerThreadJitState { ctx: JitCtx, … }`
  (or a `Box<JitCtx>`), allocated at session init. `run_compiled` passes
  `&mut *thread.ctx` to `(meta.func)`.
* Per entry write only what the guest can have changed between entries:
  `gpr[16]`, `xmm[16]`, `rflags`, `rip`, `gs_base`, `mem`, `mem_gen`, `inv_gen_baked`,
  and reset the per-run counters (`chain_depth=0`, `insn_acc=0`, `chain_hops=0`,
  `chain_store_ops=0`, `load_calls=0`, `store_calls=0`, `fault=0`,
  `gpr_dirty_bits=0`, `xmm_dirty_bits=0`, `mem_path=default`). ≈40 stores, no 4.6 KB.
* Per exit read back `gpr`/`xmm` (already mask-gated on `gpr_dirty_bits`),
  `rflags`, `rip`, and fold the counters. Nothing to copy back for the TLB.
* Also fold the pin/gen *diagnostic* block into the existing
  `if self.thread.pins_gen != mem_gen` branch — it is a pure function of `pins`, so
  the reported numbers stay bit-identical while the per-entry cost goes away.

**Why it is safe, and the invariant to state.** The invariant that keeps this honest:
*a compiled block must observe exactly the same `JitCtx` whether it was entered from
the dispatcher or from a chain edge.* Today that is guaranteed by both paths
dereferencing the same pointer for the duration of the run. With a persistent frame it
is guaranteed **structurally** — there is one frame, and both paths hand out
`&mut` to it — which is a strengthening, not a weakening. The two ways to break it are
(a) keeping a stale `RegFile` view alive while the frame changes (fix: `RegFile` is
only the authoritative source at the *call boundary*, as today; nothing else reads it
during a run), and (b) two engine threads sharing one `PerThreadJitState` (fix: it is
per-thread by construction, and `JitCtx` contains raw pointers, so the owning type
needs one audited `unsafe impl Send` site — or, cleaner, the bulk fields become
references: `tlb: &'a mut GenTlb<…>`). The alternative shape, changing `JitCtx`'s bulk
fields to references, is a peer-owned file (`jit/lower/mod.rs`), so the two shapes
should be decided together.

**How to verify it.** (1) The existing chain/dispatcher differential: `chain_hops` /
`JitCtx` chain tests, the notepad CRLF case and the Doom `movdqu` case recorded in the
comments at the XMM writeback — both were *caused* by a frame that disagreed with
itself across an entry, so they are the right regression tests. (2) A new test that runs
one block from a chain edge and from the dispatcher and asserts the resulting `RegFile`
is identical. (3) The full `WIE_CPU=iced` differential suite. (4) `insns`/`entries`
must be unchanged on every workload in §2 (they are deterministic today).

**Expected gain, honestly bounded.** The memmove component (8.9%) is provably pure
copy and goes to zero. The scalar copies (part of the remaining 23%) mostly go to
zero too. I would expect **1.25–1.4x on mx9/mx1** and less on compile-dominated guests
(`x` is 24% dispatch, so ≤1.3x even if perfect). I have *not* implemented it: the
frame type lives in `crates/wie-cpu/src/jit/lower/mod.rs`, which a peer holds this
round, and the `Send` question above is a design decision, not a typo fix. This is the
single highest-value change in the document.

### 4.2 L2 — Stop serialising every compile on one mutex (up to ~1.9x on extraction)

**Current cost.** `x`: 24% of the guest thread blocked on the engine mutex, plus 25%
inside the compile it is waiting for. `mx9` startup: 53% lock-wait. Steady-state mx9:
1%.

**Why it is serialised.** `JitShared::compile_from_kind_shared` takes
`lock_unpoisoned(&self.engine)` (`shared.rs:1137`) and calls `compile_block`, which
runs Cranelift's `define_function` — the whole ABI-level pipeline (isel + regalloc,
88% of compile time, §4.3) — while holding it. `JITModule::define_function` needs
`&mut JITModule`, so *any* concurrent compile must serialise unless there is more than
one module.

**Patch shape.** Give each compile worker its **own** `JITModule` per tier (N modules
per tier instead of one). The code base already proves the pattern: `FuncId` is
module-local and the tier design already pays for it (`chain_ids` entries carry their
tier, `chain_map_for(tier)` is the single filter). This extends the key by one
dimension — `(tier, module)` — and the invariant becomes "direct chaining is
same-tier-**and**-same-module". Everything else (per-block code cache keys, `opt_level`
records) already carries the tier tag and would gain the module tag.

Costs: N× code memory (a few MB per tier — acceptable), N× `finalize_definitions`, and
the cache key space grows. The payoff is that the guest thread stops blocking behind
other threads' compiles, which is exactly the 24% on `x`.

**Cheaper partial win, worth trying first.** The guest thread's wait is for *any*
module lock. If the guest's inline compile path took a *dedicated* module (guest
inline = module 0, workers = modules 1..N), the single-threaded case would never
contend: the workers' compiles become invisible to the guest. That is the same
mechanism with 2 modules instead of N+1 and no change to the chain-table dimension
(worker-compiled blocks would still be chained only within their own module, which is
already the tier rule). This is the version I would prototype first.

### 4.3 L3 — Cranelift's own codegen is 88% of compile time (≤1.11x, small guests only)

```
compile_block inclusive = 2846 samples (mx9 early window)
  2511 (88.2%)  cranelift AArch64Backend::compile_function  (isel + regalloc)
   121 ( 4.3%)  lower::emit::emit_body_a   (our CLIF emission)
    40 ( 1.4%)  remove_constant_phis
    31 ( 1.1%)  JITModule::finalize_definitions
```

Nothing we write dominates; the third-party ABI pipeline does. A tier-0 backend that
emits host code directly from the decoded block (no CLIF, no regalloc) would therefore
delete ~88% of compile CPU. What it must trade: emitted code quality. The cheapest
honest tier-0 is a slot-based emitter (every guest GPR op becomes ctx→reg, op, reg→ctx),
which is roughly Cranelift-without-regalloc: plausible 1.5–3x more host instructions
for the blocks that use it.

**Ceiling by class:** the guest-visible stall is 0.5% (mx9), 1.6% (mx1), 10.8% (`x`
loaded), 10.6% (`h`). Even a *free* baseline emitter is worth **1.005x on mx9** and
**1.11x on `h`**. Its real value is different: it makes short-lived guests and
one-shot blocks cheap, and it removes the CPU burn that competes with the guest on a
loaded or N-threaded host. Do it for `h`-shaped workloads, not for 7-Zip `-mx9`.

### 4.4 L4 — Never compile a block that runs once (≤1.11x, and the knob does not do it)

`h` compiles 2 087 blocks (1 893 of them **eager**, i.e. on first sight) to retire
5.53 M instructions. If a block executed once is never compiled, that compile CPU and
its 10.6% of stall disappear.

The existing knob does **not** buy this. `WIE_JIT_TARGET_WORK=1800` (the "compile on
the 2nd execution" proxy) measured on `h`, 5 interleaved pairs:

```
hs,def, 1429.45   hs,tw, 1058.13   ratio 0.740
hs,def, 1124.07   hs,tw, 1108.71   ratio 0.986
hs,def, 1109.30   hs,tw, 1110.36   ratio 1.001
hs,def, 1116.87   hs,tw, 1112.32   ratio 0.996
hs,def, 1109.34   hs,tw, 1106.22   ratio 0.997
median 0.996 (4/5 pairs)
```

Only the first pair (cold start, 1429 ms outlier) showed a win; the steady pairs are
flat. `TARGET_WORK` moves the *hotness promotion threshold*, not eager compilation, and
`h` is eager-dominated. The same knob on `xs`: ratios 0.987, 1.085, 0.952 → median
0.987 (2/3). **Conclusion: the lazy-compile lever is real but has no knob; it needs a
policy inside the promotion decision** ("compile on the second execution, never the
first"), which is a small, contained change in the miss path — and it must be gated so
the compute class, whose blocks are hot, is unaffected.

### 4.5 L5 — Emitted-code quality: `WIE_JIT_OPT` and region size

The base tier compiles at `opt_level=none` because compile cost dominates real guests
(`docs/status.md` measured 1.46–1.74x wall and 1.5–1.9x compile_us for `speed` on
`7za i`). With per-block tier-up, only self-loop blocks get `speed`. On mx9 the emitted
share is 58% of the guest thread, and mx9 has 18 self-loops (per `docs/status.md`), so
the question "is `speed` worth it for a second class of block?" is live.

Measured here on mx9s, 2 interleaved pairs (`WIE_JIT_OPT=speed` forces every block,
including the 18 LZMA self-loops, through Cranelift's full optimizer):

```
mx9s,def,   29778.22  compile_us 8490676      mx9s,speed,35912.76  compile_us 19935508   ratio 1.206
mx9s,def,   34157.02  compile_us 2588169      mx9s,speed,40491.01  compile_us 20162238   ratio 1.187
insns within 0.0001% in both arms (1 921 148 860 vs 1 921 146 608; the difference is
background-compile timing, which trades a chain edge for a dispatcher entry)
```

**`speed` costs 1.19–1.21x on the compute class.** The compile cost triples
(2.6 → 20.2 s) and buys less than that back in emitted-code quality. So the answer to
"should we optimise the emitted code harder?" is *not* "turn on the optimiser": the
1.46–1.74x win recorded in `docs/status.md` for `speed` was measured on `7za i`
(a compile-dominated startup command), and it reverses sign on `-mx9` (an
emitted-code-dominated one). The current design — `none` by default, per-block tier-up
to `speed` for hot self-loops — is the right shape, and L5 is largely *closed*: the
remaining lever there is better **lowering** (fewer CLIF ops per guest instruction),
not a different `opt_level`.

**Region formation could not be measured, and that is itself a finding.**
`WIE_JIT_SUPER=all` — the documented knob for block-wide super regions — **segfaults**
(rc=139, no output beyond `guest_args`) on `7za a -mx9`:

```
$ WIE_JIT_SUPER=all ... wie run ... 7za.exe -- a -mx9 -y 'D:\out9.7z' 'D:\rnd'
guest_args: ["a", "-mx9", "-y", "D:\\out9.7z", "D:\\rnd"]
rc=139
```

So the region lever (L7) cannot be bounded with the existing mechanism, and any work
on it must first make the super path survive a real guest. That is a correctness bug
in a diagnostic knob today, and it sits in `crates/wie-cpu/src/jit/lower/` (peer-owned
this round).

*Read this whole subsection as superseded twice, in the order it happened: the crash
was fixed on 2026-11-24 (§4.5's update below), and the `=all` **value was then deleted
from the code** in the same pass that recorded the noise measurement (§4.7).
`WIE_JIT_SUPER` survives as an off-switch only, so no arm below can be run today — the
numbers are kept because they are the reason the mode is gone.*

> **Update, 2026-11-24 — fixed, and the L7 lever is now measurable.** The crash was
> not in the lowering's super branch but in the eligibility proof that decides whether
> to emit it (`crates/wie-cpu/src/jit/block.rs::analyze_block_stack_pin`). It rejected
> a block that wrote the base register *after* the first stack access but not one that
> wrote it *before*, and the guard is computed on the base register's **entry** value.
> The shape that fired is WIE's own in-guest `GetFileSize` helper
> (`wie-runtime/src/guest_io.rs`, `getfs_impl_va + 0x17`): `mov rbp, <handle-table VA>`
> → `add rbp, rax` → `cmp [rbp], r10`. Under `SUPER=all` the entry guard passed on the
> caller's real `rbp`, while the emitted load used the table pointer, so
> `host = (pin.host_base - pin.guest_base) + addr` landed on `0x7000_e9cf_f000` — a
> guest address read as a host pointer. Reproduced deterministically with
> `WIE_JIT_SUPER=all … 7za.exe -- h D:\rnd\r0.bin` (rc=139; the faulting arm64
> instruction was `ldr x5, [x23, x0]` with
> `x23 = 0x00000000e98fc000` (the stack pin bias) and
> `x0 = 0x0000700000403000` (the handle-table guest VA)), not with `a -mx9`:
> the crash needs the accelerator helper, which is why the guest *mode* mattered
> as much as the corpus. The analysis now refuses any block that
> writes its base anywhere (plus `xchg`/`xadd` register operands, which the operand-0
> check missed), four unit tests pin the rule, and with it the super path is
> byte-exact: `a -mx1` and `a -mx9` produce identical archive MD5s with and without
> `SUPER=all` over the calibrated corpus, and the whole micro-suite passes under
> `WIE_JIT_SUPER=all`. L7 can be planned now — the `super_all` arm of
> `scripts/real-guest-perf.sh` was the harness for that, and it existed only while the
> value did.
>
> **Then the mode itself was deleted.** The measurement in §4.7 showed it
> buys nothing (1.008 / 0.971 / 0.992 / 1.061), and a byte-exact arm that cannot pay is
> a trap rather than a tool, so `WIE_JIT_SUPER` is now an off-switch only: `0`/`off`/
> `false` disables the self-loop super path, and every other value — including `all`,
> `loop` and `1` — means the default. The `super_all` arm was removed from
> `scripts/real-guest-perf.sh` in the same change; `--arms def,chain0,opt_speed` is the
> default set. There is no longer any way to turn the block-wide mode on.

### 4.6 L6 — Chaining is already the best thing in the emulator

`WIE_JIT_CHAIN=0` costs 1.53x (median, `mx1s`). Round 2 of this workstream already
proved that the *stores* inside a hop are free (removing 27–75% of
`hop_store_ops` gave 0.9955/1.0264 paired medians, reverted), so the hop's cost is the
guard + the chain-table lookup + the call itself — all already small. Do not spend
more time on the hop; spend it on the *entry* (L1) and on *fewer entries* (L7).

### 4.7 L7 — Fewer entries: the multiplier on L1

Entries per guest instruction on mx9 is **93.9** — 81.2M entries for 7.62G instructions,
measured 2026-11-25; the round that wrote this table measured 41.6/226M, so the count has
already fallen 2.3x. What an entry *costs* has moved even further: per
[`dispatcher-entry-attribution.md`](dispatcher-entry-attribution.md) it is 85 ns and the
whole entry path is now **≈5–6% of the guest thread, down from the 32%** this lever was
costed against, because L1 landed. Region formation (superblocks/traces) attacks the
*count*, which L1 did not touch:

* A 400-instruction region still divides the entry count by ~10, but 10x of a 5–6% share
  is ~5 points, i.e. **≈1.05x on mx9 — not the ≈1.4x this row was costed at**, and not
  the multiplier on L1 that made it interesting (L1 is already in). At the attribution
  doc's 85 ns/entry the 81.2M entries are ~7 s of mx9's 76 s: real, no longer dominant.
* Reconcile the count before costing anything here: the attribution doc projects its
  percentages with "≈220 M entries per run", this harness measures 81.2M for the same
  7.62G instructions. Whichever is right, one of the two is a stale binary or a
  different workload, and the L7 ceiling moves with it.
* `WIE_JIT_SUPER` only ever turned *self-loops* into a super path — and it is now an
  off-switch only (`=0` disables even that; the `=all` mode was deleted after measuring
  noise, §4.7's table below). Turning general straight-line runs into regions needs block
  discovery across edges plus exact exit stubs for faults/SMC/protect — that is why it
  is an architecture change, and it is the one that also makes the code cache more
  effective (fewer, larger units to persist).

> **Measured 2026-11-25 — `SUPER=all` is not a region lever, and the candidate win was
> noise.** With the eligibility bug in §4.5 fixed, the earlier "0.86–0.90x on mx9"
> reading (n=1, small corpus) needed confirmation on the calibrated full corpus and
> proper reps before anyone flipped a default on it. The arm existed only for this
> measurement; it — and the mode — are gone, so reproduce it from a checkout taken
> before the deletion:
>
> ```
> ./scripts/real-guest-perf.sh --classes mx9,mx1,x,h --arms def,super_all --reps 3 \
>     --max-load 6 --out /tmp/perf-A          # host load at rep start 3.07-4.10
> # ^ as it was run. The `super_all` arm is gone (removed with the mode), so this
> #   command fails today with `unknown arm: super_all` on a current checkout.
> ```
>
> | class | `def` wall (median) | `all`/`def` ratio (median, min..max) | entries `def` → `all` |
> |---|---|---|---|
> | mx9 | 76.35 s | **1.008** (0.968..1.133) | 81.2M → 85.1M |
> | mx1 | 36.10 s | **0.971** (0.938..1.053) | 25.7M → 26.2M |
> | `x` | 10.29 s | **0.992** (0.980..1.080) | 4.52M → 4.06M |
> | `h` | 3.27 s | **1.061** (1.014..1.287) | 4350 → 4271 |
>
> Every spread straddles 1.0, so the rule stated before the run — flip the default only
> if *both* load-heavy classes improve ≥5% with no class regressing — is **not** met,
> and the default stayed `loop`. (There is no `loop` value now: the whole three-way
> enum went with the mode, and the surviving default is "on for self-loops, `0` turns
> it off".) Archives are byte-identical either way (`a -mx1`
> `e136e4ea…`, `a -mx9` `04c96f0f…`, same MD5 under both arms).
>
> The reason the ratio cannot move is in the mechanism, and it corrects this section:
> **`WIE_JIT_SUPER` forms no regions.** It emits *two* paths inside one block — a
> "super" path using bare host loads when a block-wide entry guard proves the whole
> stack range, and the normal sticky/pin path otherwise — so it removes the per-access
> probe inside a block but cannot change how often a block is *entered*
> (`jit/lower/mod.rs`, `dual_super`). The only measured side effect is compile cost:
> mx9 `compile_us` 26.8M → 19.5M (−27%, less CLIF per block, no wall benefit because
> compile is background there). The entry count — the quantity L7 is about — is
> untouched on the load-heavy class (and 4% *higher*, within noise).
>
> Consequence for planning: L7 must be attacked with real region discovery across
> edges (block discovery + exit stubs), not by making `all` the default. The argument
> for keeping the knob as a byte-exactness arm (a different memory lowering for every
> stack-pin-shaped block, identical archives, micro-suite green under it) was weighed
> and **lost**: a second memory path that cannot pay is a trap, so the value and the
> arm are gone and only the off-switch is left.
>
> Regression side, same protocol: `long_loop` is 0.40 s under `all` vs 0.42 s at
> defaults (median of 6 interleaved pairs, load 2.1; budget 0.55), and the whole
> micro-suite passes under `all` — so the mode was safe, it just does not buy wall clock.

### 4.8 L8 — Host stops, resolution, handlers (≤1.2%)

Measured: `handler_ms` 0.11% (mx9), 0.22% (mx1), 1.18% (`x`), 0.50% (`h`); the top
exporter by time on `x` is `findfirstfilew` at 38 ms and `createfilew` at 30 ms, of a
9.3 s run. `resolve_ms` is 0.54 ms *total*. A completely free handler layer buys
**≤1.2%**. In-guest stubs for `malloc`/`free`/`memcpy` are pointless for wall clock
(4380 `free` calls cost 1.82 ms in total on mx9 — 0.002%); the guest's own allocator
work is already guest code. The one place handlers matter is GUI startup (36.7% of a
192 ms run), which is not a load-heavy class.

### 4.9 L9 — Threading

The measured fact for the single-threaded ceiling: **the compile mutex serialises, and
the workers are idle while the guest waits** (`x`: guest 24% lock-wait, workers 88–90%
idle). Under N guest threads the same mutex is taken for every compile from every
thread, and `chain_ids`/ready-map reads are shared. `WIE_JIT_WORKERS=1` on `mx1s`
measured 1.146 and 0.925 (median 1.03, 2 pairs) — i.e. worker *count* is not the
lever; access to the module is. Cross-thread scaling measurement is a peer's lane.

> **Cross-thread scaling: measured, §10.** The placeholder above is now filled in with a
> 5-repetition interleaved sweep of 7-Zip's own `-mmt` against a native control. Three of
> its results contradict the expectations this section was written under, and are worth
> reading before acting on L2 or on any lock: (i) at N guest threads the compile mutex is
> **4.9%** of wall on `-mx9`, not 50% — it is 32% only on `-mx1`, where guest compute is
> cheap; (ii) the big `WinApiState` lock costs **1.4 ms per 60 s** and **0.8% of sampled
> thread-time**, so partitioning it cannot pay; (iii) the emulator's threading loss at
> `-mx9` is mostly that the *second* guest thread costs **+51% host CPU for −13% wall**,
> which is cycles, not waits. Note also that `-mmt=8` starts only 2–3 real encoder threads
> on a corpus this size, so no cell in §10 is an 8-thread measurement — the native control
> gets 1.57–1.69x there, which is what the host was actually willing to give.

---

## 5. Dead ends — do not spend time here

| Idea | The number that kills it |
|---|---|
| `guest_va == host_va` mapping / a faster soft-translate fast path | `WIE_JIT_MEM=slow` (helper-only oracle) costs **+0.7% median** on mx9s (pairs 1.0135, 1.0007). The inline translation path is worth ≤1.4% on the compute class, so perfect translation buys at most that. The 4K-guest/16K-host page problem, the fake range and the software permission checks are a large risk for a ≤1.4% prize. |
| iced-residue work (`perf-plan-2` G2) | iced is 0.36% (mx9), 0.89% (mx1), 0.02% (`x`), 0.2% (`h`) of retired instructions. At iced's ~8x per-instruction cost the wall ceiling is **≤0.8%**. G2's <1% target is already met. |
| In-guest stubs / accelerator coverage for hot exports | `handler_ms` ≤1.2% of wall on every load-heavy class, and `resolve_ms` ≤0.54 ms. Boosted `malloc`/`free`/`memcpy` stubs cannot pay: 4380 `free` calls = 1.82 ms on mx9. |
| Guest-heap allocation patterns | Same number: 3877 `malloc` + 4380 `free` = 3.98 ms of 102.6 s (**0.004%**). |
| WinAPI name resolution (`resolve_winapi_id` linear scan, `is_winapi_library` list) | `resolve_ms = 0.54` on `x`, `0.00` elsewhere — it is not on the hot path at all. Making it a perfect hash changes nothing measurable. |
| The existing lazy-compile knob (`WIE_JIT_TARGET_WORK`) | 0.996 median on `h` (4/5 pairs), 0.987 on `x` (2/3). It does not gate eager first-execution compiles, which is where `h`'s cost is. |
| Chain-hop store elimination | Already measured in round 2: removing 27–75% of the hop's GPR stores gave paired medians 0.9955/1.0264 → reverted. The stores are free; the hop is not where the time is. |
| Per-thread TLB warm-start (publish a running engine's TLB, copy it into every later engine) | **Sound and still slower: 0.87 / 0.95 / 0.98** paired on `-mx9` (`-mmt=2`/`-mmt=4`), with `cpu_user` **up in every pair** (88.7 → 104.1 s). A `TlbValue` carries its generation, so a stale copy is refused as a miss rather than used — the change is correct, it just loses: the snapshot is the *pump's* working set, and the worker evicts it with its own misses. Built, measured, reverted (§10.10). |
| Generation-gated fast path for the SMC drain (`drain_pending_code_writes`) | **Worth 0.4%** — 2.59 ns saved per entry × 240 M polls ≈ 0.6 s on mx9; the pessimistic bracket (the ~7.5 ns/poll sampling figure, 1.43% inclusive) reaches 1.4–1.5%, and nothing above it. The poll takes a mutex that is never contended and finds a page **27 529 times in 239 852 130 polls (0.011%)**; `lock → take(empty) → drop` is 7.4–8.4 ns. Built behind a knob and A/B'd (89.43 → 86.85 ns/entry, median pair ratio 1.0701 on the entry-dominated guest), then **not landed**: a change to the invalidation protocol does not buy half a percent in a closing round. The 0.9% `__psynch_mutexwait` that first put it on the list is the **compile** lock — misattributed to the drain — §5 of [`dispatcher-entry-attribution.md`](dispatcher-entry-attribution.md). |
| Per-block XMM masks (skip the 256 B bank snapshot for a pure-GPR block) | **Unsound, not merely unprofitable — rejected for correctness.** A chained successor may load any live `xmm` from `JitCtx`, and the successor is chosen at runtime, so this block's mask says nothing about what the rest of the native run will read. The `uses_sse`-gated variant shipped once and wrote `0x0000` pairs for notepad's CRLF `movd [rcx], xmm0`. The snapshot it would save is 8.2 ns/entry (1.4% of mx9); a *sound* mask needs the union over the dynamic chain, and the only sound alternative — frame-authoritative bank plus a coherency flag every non-JIT writer must clear — is a cross-module invariant whose failure mode is a silent wrong answer. §3 of [`dispatcher-entry-attribution.md`](dispatcher-entry-attribution.md). |
| More compile workers | `WIE_JIT_WORKERS=1` was indistinguishable from the default on `mx1s` (1.146, 0.925 — median 1.03, opposite signs). |
| Tuning `--max-api` / stop counts | Stops are bounded by handler time (≤1.2%) and `resolve_ms`; mx9 does 20 424 stops in 102.6 s. |
| `WIE_JIT_HOTNESS_THRESHOLD` as a promotion-threshold knob | **The knob's value was never read** — `select_hot_threshold` (`jit/pipeline.rs`) used it only as an `== 0` switch for the forced-eager regime, so every non-zero value produced the same `work_weighted_threshold(pure_insns, target_work)`. Only `WIE_JIT_TARGET_WORK` moves the threshold (and it measured 0.996 / 0.987 — §4.4). Round 3 found this while looking for a cheap way to express the lazy policy. **The knob is now deleted, not just corrected in the docs**: the `== 0` switch became a plain `forced_eager: bool` parameter (`cfg!(test)` at the one call site), so there is no env var to set and the `docs/RUNBOOK.md` row is gone with it. |

---

## 6. What was implemented this round

> **Update (later the same day): L1 landed.** One persistent `JitCtx` per engine
> (`size_of` = 3744 B, so §4.1's "4.6 KB" was ~25% high) instead of that value
> being built and torn down per dispatcher entry. Measured
> **1.33x wall / 1.31x CPU on `7za a -mx9`** (median of 12 interleaved pairs,
> output byte-identical), `_platform_memmove` under `finish_compiled` **31.4% →
> 0.9%** of the guest thread, `long_loop` unchanged (0.9986). `JitCtx`'s layout
> is unaltered, so `CODE_FORMAT_VERSION` stays 3. Details, the invariant and its
> tests, and what is left in the entry path: [`l1-persistent-frame.md`](l1-persistent-frame.md).

**One change was written, measured, and reverted, and nothing shipped from it.**
That is the result, and here is the number that produced it.

**S1 — hoist the pin/gen diagnostic block out of the per-entry path** (`jit/pipeline.rs`).
The block at the top of `run_compiled` recomputed eight pin spans, `pin_allow_bits`,
`gen_bumps` and `gen_peak` on **every** dispatcher entry, although `pins` is written in
exactly one place (the refresh branch immediately above it) and `mem_gen` only advances.
The patch moved the whole block under the same `gen_moved` condition that refreshes the
pins, keeping the reported stats identical by construction. Two binaries from the same
base (`/tmp/bins/h_sym` = `2e0a1dd`, `/tmp/bins/s1_sym` = `2e0a1dd` + the patch), 3
interleaved pairs on mx9s:

```
mx9s,base, 32164.60   mx9s,s1, 33875.15    ratio 1.0532
mx9s,base, 34450.33   mx9s,s1, 34779.65    ratio 1.0096
mx9s,base, 38395.70   mx9s,s1, 36431.43    ratio 0.9488
median 1.0096 (1/3 pairs better); insns 1 921 171 081 in 5 of the 6 runs
(the sixth differs by 0.00005% — background-compile timing trades a chain edge for a
dispatcher entry)
```

> **Correction (round 3):** this round originally read "which is why entries, not wall,
> is the stable counter across arms". That is wrong, and the same paragraph's own data
> refutes it: *instructions* are the stable counter (~5e-7 relative spread), while
> **entries move ±10% between runs of the identical workload** because how many block
> edges chain versus return to the dispatcher depends on when the background compiles
> land. Measured at HEAD on the calibrated corpus, three reps of `7za a -mx9` retired
> 7 623 206 484 ± 200 instructions but reported 68.7 M, 82.1 M and 79.9 M entries. Any
> argument that leans on an entry count has to quote the spread with it.

**Reverted.** Median +1.0%, 1/3 pairs — indistinguishable from the host drift, which
moved the base arm from 32.2 s to 38.4 s across the three pairs. The useful part of the
result: ~50 instructions of scalar diagnostics are *not* where the 32% goes, which is
what the memmove evidence already said (8.9% of the guest thread is the 4.6 KB struct
copy alone). It sharpens L1: fix the copies, not the scalars.

Gates after the revert (repo tree, which also carried a peer's in-flight
`jit/lower/**` edits): `cargo fmt --all -- --check` clean; `cargo clippy --workspace
--all-targets` **0 warnings**, and the same with `-D warnings`; `cargo nextest run
--workspace` **2028/2028 passed** (1 leaky), 0 skipped; `scripts/check.sh --only
file-sizes --only dep-direction --only winapi-port --only state-bindings` all pass,
`whole-state bindings ok (439 = budget 439)`.

Everything else on the lever list is either already landed (chaining, tier-up,
`opt_level=none`), measured to be worthless (§5), or too large and too cross-cutting to
land inside a measurement round with a peer holding `jit/lower/**` (L1, L2, L3, L7).
L1 in particular is the one I would dispatch next: it is ~400–600 lines in two files, it
has a provable mechanism, and its measured share is 32% of the heaviest real workload.


---

## 7. What I could not verify, and the risks I would insist on

* **L1's `Send` question.** `JitCtx` holds raw pointers (`mem`, `chain_slots`,
  `large_free`, `tlb_hot_ptr`, …). Owning one per thread requires either one audited
  `unsafe impl Send` on the owning per-thread struct or converting the bulk fields to
  references. Both are decisions for whoever lands L1; I did not guess.
* **L2's memory cost.** N modules per tier multiplies code memory and
  `finalize_definitions` cost. I bounded the *time* win (24% on `x`) but did not
  measure the memory.
* **`x`'s 19.0 M insn/s vs mx9's 91.3 M/s.** Two thirds of that gap is explained
  (49% compile+lock on `x`) and the rest is fewer instructions per entry (19.9 vs 41.6)
  and I/O; I did not isolate the remainder.
* **All absolute times.** Host load moved mx9s between 32.5 s and 39.7 s within one
  session. Only ratios in this document should be reused.
* **GUI and game classes** are startup measurements only (§2.1). A statement about
  "GUI performance" would need a window server.
* ~~**`WIE_JIT_SUPER=all` segfaults on a real guest** (§4.5, rc=139 on `7za a -mx9`), so
  the region-formation lever has no working measurement harness.~~ **Fixed 2026-11-24**
  — see the update in §4.5. The root cause was the super-path *eligibility* proof, not
  the lowering: a block that assigns its own base register was accepted, so the
  entry-value guard proved the wrong address. `scripts/real-guest-perf.sh --arms
  def,super_all` was the harness L7 needs — and that arm, like the mode it selected,
  has since been **deleted** (the default arm set is `def,chain0,opt_speed`).
* ~~**The fixed super path is a candidate 0.86–0.90x win on mx9**~~ **Refuted
  2026-11-25.** With reps and the calibrated full corpus it is 1.008 (0.968..1.133) on
  mx9, 0.971 on mx1, 0.992 on `x`, 1.061 on `h`: noise, not a win, and the entry count
  it was supposed to attack does not move because the knob is a per-block memory path,
  not region formation — §4.7 has the table and the mechanism. Record the number,
  do not flip it — and since the mode has been **deleted** the question is closed
  anyway: `WIE_JIT_SUPER` is an off-switch only, so there is no default to flip.
* **The gates were run with a peer's in-flight edits in the tree** (`jit/lower/*`,
  `jit/block.rs`). They are green as reported in §6, but a peer's next edit can change
  that; my own change this round was a Markdown file.

---

## 8. What we must add, in order

1. **A persistent per-thread frame (L1).** One `JitCtx` per engine thread, not one per
   call. ~40 stores per entry instead of 4.6 KB of copies and ~100 scalar copies.
   Measured share 32% on mx9, 24% on `x`. Expected 1.25–1.4x on the compute and
   I/O classes. This is the single change I would dispatch first.
2. **A compile path that does not serialise the guest (L2).** Per-thread
   `JITModule`s (or at minimum a dedicated module for the guest's inline compiles).
   Measured: the guest blocks on `lock_unpoisoned(&self.engine)` 24% of `x`.
3. **A first-execution compile policy (L4).** Never compile a block on its first
   execution. Measured ceiling 1.11x on `h`, and it removes the eager compile storm
   that makes short guests expensive.
4. **Region formation (L7)** — real region discovery across edges (plus exit stubs for
   faults/SMC/protect). Measure before dispatching it: the premise that made it third
   has moved twice. Entries fell from 226M to 81.2M per mx9 run, and the entry path fell
   from 32% to ≈5–6% of the guest thread once L1 landed
   ([`dispatcher-entry-attribution.md`](dispatcher-entry-attribution.md)), so a 10x
   entry cut is now **≈1.05x**, not ≈1.4x. Not `WIE_JIT_SUPER=all`: the segfault that
   used to gate this is fixed (§4.5), but the mode is a per-block memory path rather than
   region formation, it measured 1.008 on mx9 (§4.7), and it has since been **deleted** —
   `WIE_JIT_SUPER` is an off-switch only, so L7 is not a knob flip at all.
5. **A real-guest perf harness.** `./scripts/run-micro-suite.sh` and `--only perf` gate
   `long_loop`, a compute micro that tells us nothing about file I/O, compilation or
   allocation. Everything in this document came from ad-hoc scripts plus the
   archive-in-the-bottle hazard in §1. **Landed 2026-11-24** as
   `scripts/real-guest-perf.sh`: deterministic corpus generator + CRC manifest
   (`--make-corpus`, verified on every run), every archive written under `--corpus` and
   deleted there (a corpus inside the guest bottle is refused, a run whose archive
   survives the delete is refused), interleaved reps with a per-(class, arm) cache
   warmup so no arm pays a cold compile the baseline did not, per-class ratios from the
   median of per-rep pairs, and the `scripts/check.sh --only perf` load convention
   (`WIE_PERF_MAX_LOAD`, default 2.0) — over the limit a rep is skipped and says so, and
   a sweep with no measurable rep exits non-zero instead of reporting the host.
6. **Not a lever, but a prerequisite for trusting any of this:** the differential
   suites (`WIE_CPU=iced`, `WIE_JIT_MEM=slow`, chain tests) must stay green through
   L1/L2, because both change the frame/ABI boundary where this project's worst
   historical bugs (notepad's CRLF `movd` zeros, Doom Retro's dropped `xmm` def) lived.

## 9. Appendix — reproduce

```bash
# symbol-bearing release binary (the shipped profile is stripped)
CARGO_PROFILE_RELEASE_STRIP=false CARGO_PROFILE_RELEASE_DEBUG=1 cargo build --release

# the checked-in harness: corpus, interleaved arms, per-class ratios
./scripts/real-guest-perf.sh --make-corpus /tmp/wie-perf-corpus
./scripts/real-guest-perf.sh                       # mx1s, mx9s, h x def/chain0/opt_speed
./scripts/real-guest-perf.sh --classes mx9,mx1 --reps 2

# one workload, one run, counters (no slice knob needed since 2026-11-24: the
# watchdog counts consecutive slices and its default covers mx9's 84-slice peak)
WIE_RUNTIME_PROFILE=1 ./target/release/wie run \
    --max-api 40000000 --drive-d /tmp/corpus real_exes/7za.exe -- \
    a -mx9 -y 'D:\out9.7z' 'D:\rnd' 'D:\txt' 'D:\mid'

# stack attribution (needs the symbol-bearing binary)
/usr/bin/sample <pid> 5 -file /tmp/sym.txt

# interleaved A/B of one knob (arms: label + env spec)
./ab.sh 3 mx9s def '' slow 'WIE_JIT_MEM=slow'
```

Corpus (`/tmp/corpus`, deterministic, seed 20261124): `rnd/r0.bin`, `rnd/r1.bin`
(2 MB each, incompressible), `txt/t0.txt` (2 MB, highly compressible), `mid/m0.bin`
(1 MB). The 1.3 MB subset `/tmp/corpus_s` (512 kB + 512 kB + 256 kB) is the same mix
scaled down for knob ablations.

---

## 10. Threading — does a real multithreaded app scale? (measured)

L9 above is the single-threaded half of this question: one guest thread, blocked on the
compile mutex. This section is the other half — **N guest threads, same real app**, and
the `-mmt` knob is 7-Zip's own, so the workload is the app's own parallel code, not a
benchmark's. Instrument: `scripts/mt-scaling.sh` (new; interleaves the emulator arm with
a **native 7-Zip control** in the same window, so "the emulator loses X" is separable
from "the host was busy").

### 10.1 First: `-mmt` does not mean N threads on this corpus, and that is not a detail

7-Zip caps its encoder threads at the number of solid *blocks*, and blocks come from the
data size and `-ms`. Measured directly (`WIE_MT_DEBUG=1`, counting `worker_main start`):

| `-mmt` | guest worker threads created, `-mx1` | `-mx9 -ms=64k` |
|---|---|---|
| 8 | **3** | **2** |

The same count comes back from the profile: at `-mmt` 2, 4 and 8 the guest retires an
identical 3.870 G instructions and makes an identical 244 `WaitForSingleObject` calls.
So **every `-mmt≥2` cell below is a 2–3 thread cell**; `-mmt=8` is not an 8-thread
measurement and this document does not claim it is. Two consequences:

* The curve below is a **1 → 2–3 thread** curve, which is exactly where an emulator's
  threading either works or does not.
* An honest 8-thread ceiling needs a corpus large enough (or `-ms` small enough) that
  7-Zip really starts 8 encoders. That was not available: this Mac is **4 performance +
  4 efficiency cores** and peers held it at load 3.3–7.4 for the whole session.
  **Correction, §10.9:** the native control's 1.57–1.69x is *7-Zip's own* block-level
  parallelism ceiling on a 2.9 MB corpus, **not** the host's. Measured separately, the
  host delivers **2.00x at 2 threads and 7.7x at 8** on pure compute. Reading the native
  arm as a host capacity was wrong; it is a like-for-like workload control, and that is
  all it is used for below.

### 10.2 The curve

Release `2e0a1dd`, `WIE_RUNTIME_PROFILE=1`, corpus 2.9 MB / 13 files (mixed
compressibility: 48% text, 12% random, 24% structured, 16% interleaved), private
`WIE_CACHE_DIR` warmed once per level, `WIE_NO_HOOK_SLICES=1000000` (see §10.7),
**5 interleaved repetitions** (repetitions outermost, whole matrix re-walked), medians:

| level | threads | wall median | cpu% | native wall | native speedup | **emulator speedup** | paired n |
|---|---|---|---|---|---|---|---|
| `-mx1` | 1 | 15.03 s | 100.6% | 97 ms | 1.00 | 1.00 | — |
| `-mx1` | 2 | 14.42 s | 163.3% | 68 ms | 1.52 | 0.996 | 5 |
| `-mx1` | 4 | 13.14 s | 178.1% | 63 ms | 1.57 | **1.14** | 5 |
| `-mx1` | 8 | 12.35 s | 182.7% | 63 ms | 1.57 | **1.20** | 5 |
| `-mx9` | 1 | 64.12 s | 99.5% | 485 ms | 1.00 | 1.00 | — |
| `-mx9` | 2 | 55.65 s | 174.2% | 310 ms | 1.61 | **1.12** | 5 |
| `-mx9` | 4 | 59.90 s | 174.3% | 333 ms | 1.54 | **1.07** | 5 |
| `-mx9` | 8 | 59.86 s | 174.8% | 292 ms | 1.69 | **1.03** | 5 |

Speedups are the median of per-repetition ratios `T(1)/T(N)`, which is what makes them
robust to the load moving under the sweep (load averages during the sweep: 2.98–6.22,
median 4.2). They are therefore **not** the ratio of the two median columns beside them;
dividing those gives a slightly different number (e.g. `-mx9` threads=2: 485/310 = 1.56
against the paired 1.61), because the medians of two noisy columns do not divide into the
median of their ratios. Cost per added thread, i.e. `T(N)·N / T(1)` — **the number the brief asks
for**, because it shows what each additional thread *costs* rather than what it buys:

| level | threads | emulator cost/thread | native cost/thread |
|---|---|---|---|
| `-mx1` | 1 | 1.00 | 1.00 |
| `-mx1` | 2 | 2.01 | 1.32 |
| `-mx1` | 4 | 3.50 | 2.56 |
| `-mx1` | 8 | **6.67** | 5.08 |
| `-mx9` | 1 | 1.00 | 1.00 |
| `-mx9` | 2 | 1.78 | 1.24 |
| `-mx9` | 4 | 3.74 | 2.60 |
| `-mx9` | 8 | **7.75** | 4.73 |

**Read it as: a thread that buys 1.03–1.20x of throughput is being charged 6.7–7.8x the
single-thread price.** Absolute slowness is not the story (the emulator is 130–155x
slower than native at `-mmt=1`); the story is that the ratio *gets worse* as threads are
added, monotonically, at both compression levels.

### 10.3 Where the loss is *not*: the locks

Two independent instruments, both at `-mmt=8`:

| instrument | `WinApiState` (the big lock) |
|---|---|
| `WIE_RUNTIME_PROFILE=1` `winapi_lock_wait` (`lock_wait`, `mt_runtime.rs:142`) | **1.43 ms of guest wait over a 59.8 s run** (`-mx9`); 0.40 ms on `-mx1`; worst single acquisition **0.027 ms** |
| `/usr/bin/sample`, 24 s of two 12 s windows | **1091 of 134 539 leaf samples = 0.81%** of all sampled thread-time, and **3.2%** of the busiest thread's time |

So the answer the previous round could not give: **at N guest threads the big
`Mutex<WinApiState>` costs ~0.002% of wall clock. It is not a bottleneck, it is not close
to being one, and no amount of partitioning it would pay for itself.** `lock_order.rs`
ranks it `State`, outermost, load-bearing for correctness (the `State → Queue → Channel`
order is what keeps the macOS UI thread from deadlocking) — and it is *performance*
irrelevant on a console workload.

Full leaf classification, same two windows (buckets exclusive, first match wins):

| bucket | `-mmt=1` | `-mmt=8` |
|---|---|---|
| `__psynch_cvwait` / condvar | 78.05% | 76.45% |
| mutex (`__psynch_mutexwait`, `pthread_mutex_lock`) | **0.11%** | **0.81%** |
| Cranelift | **0.05%** | **0.03%** |
| emitted code (anonymous JIT pages) | 9.47% | 5.97% |
| `_platform_memmove` | 6.11% | 8.00% |
| JIT memory helper (`wie_jit_load`, TLB) | 1.71% | 1.97% |
| other (`wie` + libs) | 4.24% | 6.32% |

Of the **running** (non-parked) 22.5% of thread-time at `-mmt=8`, the split is
`memmove` 35.6%, other 28.1%, emitted code 26.6%, memory helper 8.8%, **mutex 0.8%**.

Naming the mutexes, and why the sample cannot go further: every mutex sample's caller
frame collapses onto `DYLD-STUB$$memcpy`, because the waiter is *inside emitted guest
code* and `sample` resolves the caller to the nearest exported symbol of an anonymous
JIT page. So attribution to `JitShared::engine` vs `GuestHeap` vs the `RwLock<GuestMemory>`
read refcount has to come from the counters, not the stack:

| lock | in `lock_order.rs`? | measured cost at N=8 | avoidable in principle? |
|---|---|---|---|
| `Mutex<WinApiState>` | **yes** — `Lock::State`, outermost | 1.43 ms / 59.8 s (0.002%) | No. Load-bearing order; and it is free. |
| `JitShared::engine` (`shared.rs:1137`), held across `compile_block` | no (its order — guest memory → engine — is prose in `shared.rs`) | Cranelift **0.03%** of samples; `bg_stall_us` **2.93 s = 4.9%** of wall at `-mx9`, **3.95 s = 32%** at `-mx1` | Yes, but see §10.5 — the ceiling is 5% here, not 50%. |
| `Mutex<GuestHeap>` (heap fast path, `quantum.rs:370`) | no | `malloc`/`free` together are 0.004% of wall (peer's §4.9); sample shows `hash/alloc` 0.16% | Yes, and pointless. |
| `RwLock<GuestMemory>` read | no | memory helper 8.8% of *running* time, and it is shared-read | Only via per-thread arenas — see §10.6. |
| `MessageQueue`, `PresentChannel` | yes — ranks 2, 3 | not reached by a console workload | — |

The two `lock_order.rs` locks that a console workload touches at all are the two it
cannot avoid; the locks that a *console* workload could avoid are not contended.

### 10.4 Where the loss *is*: the price of the second thread

`-mx9`, profile medians (`n=5`):

| threads | wall | cpu_user | cpu_sys | guest insns (JIT-retired) | compile CPU | guest waits for compile |
|---|---|---|---|---|---|---|
| 1 | 64.12 s | 63.90 s | 0.19 s | 4.182 G | 2.56 s (4.0%) | 2.19 s (3.4%) |
| 2 | 55.60 s | 96.70 s | 0.32 s | 3.870 G | 3.08 s (5.5%) | 2.74 s (4.9%) |
| 4 | 59.84 s | 104.49 s | 0.41 s | 3.870 G | 3.20 s (5.3%) | 2.82 s (4.7%) |
| 8 | 59.81 s | 102.38 s | 0.30 s | 3.870 G | 3.37 s (5.6%) | 2.93 s (4.9%) |

The row that matters is **threads=2**: the guest retires *7.5% fewer* instructions
(4.182 G → 3.870 G) and burns **51% more CPU** (63.90 s → 96.70 s). Wall drops only 13%
(64.12 s → 55.60 s). From 2 threads onwards nothing changes at all —
CPU, instruction count, compile time and wall are all flat. So:

* The overlap itself works: 102.4 s of CPU inside 59.8 s of wall is 1.75 cores busy, and
  `wall ≈ cpu/1.75` holds at every thread count. The scheduler is not serialising the
  guest.
* **The price per unit of guest work rises ~1.7x the moment a second guest thread
  exists**, and that is the whole loss. Same 3.87 G instructions, 1.51x the CPU.
* The mix of work per instruction does not change (§10.3 table, `-mmt=1` vs `-mmt=8` are
  within noise of each other), so this is not "the emulator does something extra when
  threads appear" — it is **the same work costing more host cycles**.

The three candidate explanations, and what rules each in or out:

| hypothesis | verdict from the data |
|---|---|
| Lock contention | **Ruled out.** 0.81% of samples, 1.43 ms of instrumented wait (§10.3). |
| Compile serialisation on `JitShared::engine` | **Ruled out as the main term.** Cranelift is 0.03% of samples at N=8 and `bg_stall` is 4.9% of wall at `-mx9`. It *is* the dominant term at `-mx1` (32% of wall) — see §10.5. |
| Host-core heterogeneity | ~~**Consistent with everything.**~~ **Refuted in §10.9.** The host delivers a clean **2.00x at 2 threads** and 7.7x at 8 on pure compute, and no QoS class changes it; there is no placement penalty at the thread counts where the CPU inflation appears. |
| Shared-cache pressure | **Still open, but not where it looked.** The one per-entry shared write (`JitShared::guest_insns`) was removed in a scratch build and is **neutral** (§10.9). What *is* measurable is that the spawned worker spends **~3.9x the host CPU per unit of work** than the pump, 4x of it on the guest-memory helper path — a cold per-thread translation path, not contention. |

Those two were hypotheses when this section was written; **both have since been tested**
(§10.9): placement is refuted by the host capacity curve, and removing the one per-entry
shared write is measured neutral. What the same measurement *does* isolate is that the
spawned worker thread costs ~3.9x the host CPU per unit of work it takes off the pump,
which points at per-thread translation warmth rather than at either hypothesis — see
§10.9 for the per-thread split and §10.8 for the next experiment.

### 10.5 The compile serialisation, bounded under real N

`WIE_JIT_WORKERS` at `-mx9 -mmt=8`, 3 interleaved repetitions each (a window in which the
host load ran 3.9–39.7, i.e. too noisy for absolutes; the ordering is 3/3 vs 3/3, so the
sign is unambiguous):

| `WIE_JIT_WORKERS` | wall (3 reps) | median |
|---|---|---|
| 0 (compiles on the visiting thread) | 100.13, 82.61, 69.25 s | **82.6 s** |
| 1 | 76.31, 109.49, 80.60 s | **80.6 s** |
| 4 (default) | 42.67, 44.81, 41.08 s | **44.8 s** |

The background pool is worth **1.7–1.8x** on this workload (median-to-median 1.84;
min-to-min 69.25/41.08 = 1.68), and **every** `workers=4` run beat **every** `workers=0`
run. So the compile path is not a 5% curiosity once threads exist: serialising it on the
visiting thread costs most of the wall clock, and the existing pool is what hides it.
That is also the bound on per-thread `JITModule`s as a lever: they can only recover the
part the pool does not already hide, and `bg_stall_us` says that part is **4.9% of wall at
`-mx9`** (32% at `-mx1`, where guest compute is cheap and compile is not).

Ceiling for per-thread `JITModule`s + late-bound cross-module calls, per level:
**≤1.05x on `-mx9`, ≤1.4x on `-mx1`.** Effort L, risk high (`FuncId` is module-local;
the chain table gains a module dimension — the peer's §4.2 L2 analysis of that risk
applies unchanged).

### 10.6 Levers, with ceilings

| lever | measured cost now | ceiling | effort | risk |
|---|---|---|---|---|
| Partition `WinApiState` per-thread / shard it | 1.43 ms per 60 s at N=8 (0.002%) | **≤1.001x** | L (touches every handler) | High: `lock_order.rs` is the correctness backbone and `ThreadState.active` is process-global |
| Eliminate compile serialisation (per-thread `JITModule` + late-bound cross-module calls) | Cranelift 0.03% of samples; `bg_stall` 4.9% of wall (`-mx9`) / 32% (`-mx1`) | **≤1.05x `-mx9`, ≤1.4x `-mx1`** | L | High (`FuncId` module-local; chain table) |
| Epoch/RCU read path for shared state (`RwLock<GuestMemory>`) | shared-read; memory helper 8.8% of *running* time | ≤1.09x if the read refcount is what costs | M–L | Medium (generation/TLB invalidation correctness) |
| Per-thread heap shards | `HeapAlloc`/`Free` = 0.004% of wall | **≤1.0001x** | M | Low, and not worth it |
| Park/wake cost (`WaitTarget::wait`, 50 ms slices, `park_brief`'s 14 × `yield_now`) | 244 guest waits per run; `host_stops` 11 862; sys time 0.30 s of 59.8 s | ≤1.005x on this class | S–M | Low — but nothing to win on a compression workload |
| Thread creation (1:1 host threads + per-thread engine) | 3 guest workers × `CreateThread`; not in the top-14 exports | unmeasurable at this scale | S | Low |
| **The price of the 2nd thread itself** (§10.4) | **+51% CPU for −13% wall at threads=2** | **1.5x on this class** — the single largest term in the table | — | — (this is the one to chase) |

The last row is not a lever yet, it is the *finding*: the biggest threading term in the
emulator is not a lock, not Cranelift, and not the heap. It is that the second guest
thread costs half again as much host CPU for the same work, and everything visible in the
sample says that price is paid in cycles, not in waits.

### 10.7 Two measurement hazards this section had to work around

* **`WIE_NO_HOOK_SLICES` default (`40`) kills real single-threaded runs.** `7za a -mx9`
  over a multi-MB file legitimately runs ~800 M guest instructions with no Win32 call;
  at the default the run dies with `emulation stopped without hitting fake API hook after
  40 slices` and returns a *fast* wall time. This is the single easiest way to produce a
  beautiful, wrong scaling curve (the 1-thread arm is killed early and looks cheap).
  Every number above ran with `WIE_NO_HOOK_SLICES=1000000`. Worth asking whether 40 is
  the right default for a real app.
* **Archive reuse.** The peer's §1 hazard applies here too: every run in this section
  writes `D:\out.7z` (host `--drive-d`) and removes it first, so each run compresses from
  scratch. `7za a -y` over an existing archive *updates* it and the instruction count
  falls ~4x.

### 10.8 What this section does not establish, and what to run next

1. **A genuine 8-thread point.** Needs a corpus large enough that 7-Zip starts 8
   encoders (or `-ms` small enough), on a host with 8 idle *comparable* cores. Until
   then "the emulator does not scale to 8" is **not** what was measured; "it does not
   scale to 2–3, where the native control gets 1.57–1.69x" is.
2. ~~**E-core vs cache pressure.** Pin the guest's host threads to the 4 performance
   cores and re-run the 1→2 comparison.~~ **Done — §10.9.** Placement is refuted (the host
   gives 2.00x at 2 threads and QoS does not pin), and the per-entry shared write is
   measured neutral. The replacement lead is per-thread translation warmth: seed the new
   engine's TLB from its entry block, or reuse a retired engine, then re-run the same
   `-mmt=1` vs `-mmt=2` pair.
3. **The handoff path itself.** `WaitForSingleObject(INFINITE)` is sliced into 50 ms
   chunks (`quantum.rs:824`), each re-check taking `WinApiState`. At 244 handoffs per run
   that is invisible; on a workload with millions of handoffs it would not be.
4. **`emu_ms` is unusable for N threads.** The profile's `emu_ms` is the *primary*
   thread's accounted time only (measured: 3.1 s of emu inside an 18.6 s wall at
   `-mmt=2`). Every scaling figure above therefore uses wall clock and process `cpu%`.
   Merging worker `emu_ms` the way `JitStats` is already merged would make the profile
   honest at N threads, and is cheap.

**Nothing was shipped this round.** The sample does not justify a lock removal: the two
`lock_order.rs` locks that a console workload touches cost 0.002% and 0.8% of thread-time
respectively, and the non-`lock_order` locks are further down than that. The one thing
this section produced for the tree is the instrument, `scripts/mt-scaling.sh`, so the
next round can re-run the curve — including the native control — instead of trusting it.

### 10.9 The E-core hypothesis: tested, and refuted — plus the cache line that looked guilty

§10.4 left one mechanism untested: "the second guest thread lands on an E-core and costs
~2.5–3x the cycles per instruction". That is now measured, three ways. **It is wrong**,
and so is the reading of the native control it rested on.

#### The lever, and why it cannot even pose the question

macOS offers two candidate knobs. Built outside the repo (`/tmp/qos/qos_exec.c`), since
`wie-runtime`/`wie-cli` do not own `unsafe`:

```c
// qos_exec <ui|uin|def|util|bg> <prog> [args...]
pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE /* or BACKGROUND */, 0);
execv(argv[2], argv + 2);        // new threads inherit the creator's class
```

QoS survives the `exec` and new threads inherit it — validated on a native 8-thread
integer burn, where the arms are indistinguishable:

| QoS arm | `def` | `ui` | `uin` | `util` | `bg` |
|---|---|---|---|---|---|
| 8 threads, 2e9 iters each (wall) | 3.36 s | 3.40 s | 3.38 s | 3.05 s | 3.59 s |

**QoS does not pin on this machine.** `BACKGROUND` — which is documented to prefer
efficiency cores — costs 7% where a real cluster restriction would cost 2–3x. So the
lever cannot force placement, and any conclusion drawn from it would be an artefact of
the OS ignoring the hint.

#### The premise fails anyway: the host delivers a clean 2.00x at 2 threads

Which is the control the hypothesis actually needed — the host's own capacity curve,
same binary, fixed work per thread:

| threads | 1 | 2 | 4 | 8 |
|---|---|---|---|---|
| wall (s) | 2.77 | 2.77 | 2.63 | 2.89–3.41 |
| total work / wall | 1.00x | **2.00x** | **4.21x** | **7.7x** |

Two threads get **2.00x**, not 1.2x. Efficiency cores cost ~4% at 8 threads. At the
thread counts where the emulator loses 51% of its CPU, **the host has no placement
penalty to explain it**.

So: *is forcing QoS ours to fix?* The question dissolves — placement is not the
mechanism, so there is nothing to force. It would also be the **wrong** thing to ship:
a user deliberately spreading 8 threads over a 4P+4E part is a scheduling reality, and
an emulator overriding QoS process-wide would silently outrank the guest's own intent
and starve interactive guests. If this ever becomes a real effect, the answer is a knob,
not a default.

#### The line that looked guilty, and the negative result

With placement gone, the remaining suspect was shared-memory coherence, and there is
exactly one shared line **written once per compiled-block execution by every engine**:
`JitShared::guest_insns`, an `AtomicU64` in `shared.rs`, `fetch_add`ed from
`run_compiled` (93 M dispatcher entries at `-mmt=2`, 225 M per the peer's count) purely
to answer `is_boot_mode()` — a question whose answer stops changing after 5 M
instructions. That is one contended RMW per ~41 guest instructions, on the one line every
engine shares.

Tested in a **scratch clone** (never the shared tree — `shared.rs`/`pipeline.rs` were
being rewritten by a peer throughout this experiment), as the *upper bound* of the idea:
skip the RMW entirely once the gate closes.

| `-mx1` | base median | exp median | paired median | verdict |
|---|---|---|---|---|
| `-mmt=1` | 15.87 s | 16.11 s | 1.00 (4/4 within ±2%) | neutral, as designed |
| `-mmt=2` | 18.18 s | 18.26 s | **0.99** | **negative** |
| `-mmt=8` | 22.90 s | 21.63 s | 1.09 (3/4 pairs, p=0.31) | inside the noise |

Executed work is provably identical (`insns` = 949 150 672 in **both** arms), and the
`-mmt=8` window had `cpu_sys` swinging 5.5–10.0 s run to run, so 9% at 3/4 pairs is not a
result. **The per-entry shared write costs nothing measurable.** Nothing was shipped.

#### Where the extra cycles actually are: one thread is 3.9x less efficient than the other

Per-thread `ps -M` and `sample` of the same `-mx9 -mmt=2` run, mid-flight:

| | pump (thread 0) | guest worker (thread 6) |
|---|---|---|
| busy | 96.3% of a core | 67.8% of a core |
| emitted code | 38.0% | 33.0% |
| `_platform_memmove` | 30.7% | 14.0% |
| JIT memory helper | **3.6%** | **14.5%** |
| condvar (parked/waking) | 0.0% | 25.6% |
| mutex | 1.5% | 1.2% |

The pump gives up 11.3 s of its own work (emu 64.07 s alone → 52.81 s) and the worker
burns ~43.9 s of CPU to absorb it: **the worker spends ~3.9x the host CPU per unit of the
job it takes**. That is the inflation, and it is not contention *with* the pump — mutex
holds are 1.2–1.5% for both. It is the worker's own execution being 4x more expensive on
the guest-memory helper path (14.5% vs 3.6% of its time), which is the soft-translate /
TLB-miss path, i.e. **per-thread state that a freshly spawned engine has not warmed**.

That is now the leading hypothesis, and unlike the last one it is testable in a contained
way: per-thread TLB warm-up on engine start (seed the TLB from the entry block), or
reusing a retired engine instead of building a cold one. Both are `wie-cpu`-local. Note
this is also *consistent* with the aggregate composition being flat
(`-mmt=1` vs `-mmt=8` differ by ≤2 points in every bucket): the emulator does the same
mix of work, but a third of it runs through a colder translation path.

#### What changed since §10.2, and one honest caveat about that table

All of §10.2's numbers are from `2e0a1dd`. HEAD is now `94621e2` ("Persistent JIT frame:
1.33x on a real app"), which makes the single-threaded path much faster — measured here at
`-mx1 -mmt=1`, **27.3 s → 15.8 s (1.73x)** on the same corpus and comparable load. Its
multithreaded numbers improve too, but by less (`-mmt=8`: 25.3 s → 22.9 s, 1.10x), so the
*relative* multithread penalty got **worse**, not better: at HEAD, `-mx1` goes
15.93 → 17.38 → 22.06 → 22.87 s for `-mmt` 1/2/4/8, i.e. **adding threads now costs 44% of
wall clock**, where on `2e0a1dd` it cost 18%. A cross-commit comparison under a load that
moved between the arms (4.2–5.1 for HEAD, 4.6–16.3 for `2e0a1dd`) is weak evidence, and
this one is flagged as such — but the direction is worth a proper A/B on a quiet host,
because it says the frame change bought its 1.73x single-threaded and paid some of it
back at N threads. It also means §10.6's ceilings should be re-derived on HEAD before
anyone acts on them.

### 10.10 Closing round: the lead tested, the regression settled, the kernel time named

The dispatcher-entry fix (`8b75698`, frame marshalling 32% → 5-6%) changed the profile enough
that §10.4–§10.9's ranking had to be re-derived before acting on any of it. Re-sampled
first, then tested the lead, then settled the flagged regression. **Two of §10.9's numbers
were wrong and are corrected here.**

#### The profile after the entry fix (re-ranked, `-mx9 -mmt=2`, two 12 s windows)

| bucket | `-mmt=2` now | was (§10.3, pre-fix) |
|---|---|---|
| condvar (parked threads) | 70.5% | 76.5% |
| emitted code | 10.7% | 6.0% |
| `_platform_memmove` | **0.33%** | 8.00% |
| mutex (`__psynch_mutexwait`) | 2.05% | 0.81% |
| semaphore / other kernel | 0.99% | 0.29% |
| Cranelift | 0.06% | 0.03% |
| translation path (`mem.rs`/`tlb.rs` tags) | **~2.6% of the worker's leaves** | (was reported as 14.5%) |

`memmove` collapsing from 8% to 0.3% is the entry fix, visible from the other side: the
frame copy *was* the memmove. Emitted code is now the largest running bucket, mutex wait is
the second largest thing that is not the guest's own work, and Cranelift remains
irrelevant at steady state. Note the parked share (70.5%) is not a defect: it is a thread
pool with 2-3 working threads on 7 host threads.

**Correction 1.** §10.9's "the worker's memory-helper share is 24.3% vs the pump's 5.5%,
4.4x" is an artefact of that section's bucket definition, which matched the substring
`resolve` and therefore swept every dispatcher frame that inlines `resolve_fake_api_at`
into "memory helper". Re-attributed by `sample`'s `file:line` tags, the worker's
translation time is **~2.6% of its leaves**, not 24%. The asymmetry between pump and worker
is real but small, and the "3.9x less efficient" figure built on it does not survive: at
current HEAD `ps -M` shows the pump at 95.7% of a core and the worker at **28.6%**, so the
worker contributes ~19 s of CPU to a 65 s run, not 44 s.

#### The lever: seeding a new engine's TLB — implemented, measured, rejected

§10.9's lead was "per-thread translation warmth". The cheap fix is to publish a running
engine's TLB in `JitShared` and copy it into every engine that starts later. Written and
built (in a scratch clone, since `pipeline.rs` was being edited by a peer):

* `JitShared::seed_tlb: Mutex<Option<GenTlb<…>>>`, published once by the first engine past
  2 M retired instructions (`publish_seed_tlb`, first-writer-wins);
* `PerThreadJitState::new` copies it into `ctx.tlb` when present;
* ~40 lines, two files (`shared.rs`, `cpu_engine.rs`), no `unsafe`, no new lock.

**Soundness, which is the reason this could be tried at all:** a `TlbValue` carries the
guest range, the host base, the software permission bits and the `GuestMemory` generation;
all four are process-wide (one shared `RwLock<GuestMemory>`, one generation counter), and
`GenTlb::lookup` admits an entry only when its stamp equals the caller's current
generation. A copy taken before a map/unmap/protect is therefore *refused*, so a stale copy
is a miss and never a wrong address. No pin needs to transfer, because the generation bump
that unmaps a region is the same event that invalidates every copy. **It is sound and it
still does not pay:**

| `-mx9` | base median | warm-start median | paired | `cpu_user` |
|---|---|---|---|---|
| `-mmt=2` (n=2 clean) | 51.5 s | 59.3 s | **0.87, 1.67** | 88.7 → 104.1 s |
| `-mmt=4` (n=2) | 52.9, 53.5 s | 56.0, 54.5 s | **0.95, 0.98** | 90.6 → 96.7 s |

Three of four pairs favour the base and the fourth is the one whose base arm ran at load
10.5 against the experimental arm's 19.5 — i.e. a starved run, not a win. The change raises
`cpu_user` in every pair. Plausible mechanism: the snapshot is the *pump's* working set, so
the worker starts with a table full of pages it does not touch, and those entries are
evicted by its own misses — the warm start costs conflict misses instead of cold ones.
**Reverted; nothing shipped.** The lead is dead at this size, and a `-mmt=2` worker at 29%
of a core has little to recover anyway.

#### The regression: settled, and it was not one

Interleaved adjacent pairs, current HEAD against `94621e2^` (`2e0a1dd`), same corpus,
repetitions interleaved, load 3.8–5.4 in the clean repetition:

| cell | HEAD | `2e0a1dd` | ratio |
|---|---|---|---|
| `-mx1 -mmt=1` | 16.08, 16.66 s | 28.55, 29.61 s | **1.78x** |
| `-mx1 -mmt=8` | 19.56 s | 34.19 s | **1.75x** |
| `-mx9 -mmt=1` | 54.58, 68.88 s | 117.88, 132.24 s | **2.16x, 1.92x** |
| `-mx9 -mmt=8` | 66.26, 57.22 s | 108.45 s | **1.64x** |

**The single-threaded win is intact and larger than claimed** (1.78x at `-mx1`, 2.16x at
`-mx9`, against the commit message's 1.33x), and **the multithread case did not regress** —
HEAD is 1.6–1.75x faster at `-mmt=8` too.

**Correction 2.** §10.9's "adding threads now costs 44% of wall clock, where on `2e0a1dd` it
cost 18%" does not reproduce. Measured in the same window, both commits pay about the same
relative penalty: HEAD 16.08 → 19.56 s (**+22%**), `2e0a1dd` 28.55 → 34.19 s (**+20%**). The
44% came from comparing HEAD's post-fix baseline against a pre-fix arm that ran in a
different load window — exactly the error the flag said it might be. The multithread
penalty is a property of the emulator's threading, not of the frame change.

#### The `cpu_sys` that "appeared from nowhere": named, and it is not new

It is bimodal, in **both** commits and at both levels — the same config lands either near
0.2–1.7 s or near 5–10 s: `-mx1 -mmt=8` gave HEAD 6.87 / 5.76 s and `2e0a1dd` 10.65 / 1.48 s;
`-mx9 -mmt=8` gave HEAD 9.59 / 1.71 s and `2e0a1dd` 1.75 s. So it is a *mode*, not a
regression, and not a property of the frame change.

Three 4 s samples of a `-mx1 -mmt=8` run (3.77 s `cpu_sys` of 19.6 s wall) name it:

| kernel frame | why it is there |
|---|---|
| `__psynch_mutexwait`, `__psynch_mutexdrop` | contended host `pthread_mutex` (the biggest single term) |
| `__psynch_cvwait`, `__semwait_signal` | condvar waits — parked guest threads, and `sync_obj::park_brief`'s `wait_ms(1)` |
| `mach_msg2_trap` | the messenger/thread-port traffic that accompanies wakeups |
| `__open`, `__fcntl`, `__rename`, `closehandle` (14 calls) | the VFS, and **not** the cause: 13 files means ~14 handle operations regardless of thread count |

So the kernel time is **host synchronisation traffic, not emulator syscalls and not file
I/O**. The suspect that fits both the magnitude and the thread-count dependence is
`sync_obj::park_brief` (`wie-winapi/src/sync_obj.rs:1103`): a loser on a guest
`CRITICAL_SECTION` does 14 `sched_yield()` calls and then a **1 ms-granular** timed wait,
per attempt. 7-Zip's allocator serialises on exactly such a section from every encoder
thread, so at `-mmt=8` that is a syscall ladder being climbed thousands of times a second
across 6 threads. That is a hypothesis with a mechanism and a file, not a measurement —
**it needs the same interleaved A/B any other lever does, and there was no room this
round**, so nothing was changed.

#### The threading ledger, after all of it

| lever | status | the number that decides it |
|---|---|---|
| Partition/shard `WinApiState` | **dead** | 1.4 ms of guest wait per 60 s; mutex waits 2.05% of samples |
| Per-thread `JITModule` (kill compile serialisation) | **bounded** | Cranelift 0.06% of samples; `bg_stall` 4.9% of wall at `-mx9` |
| Per-thread heap shards | **dead** | `HeapAlloc`/`Free` 0.004% of wall |
| Warm-start TLB seeding | **dead (§10.10)** | 0.87 / 0.95 / 0.98 paired, and `cpu_user` up in every pair |
| Dispatcher-entry frame marshalling | **shipped, 32% → 5-6%** | `memmove` 8.0% → 0.33% of samples |
| Park/wake ladder (`park_brief`: 14 yields + 1 ms wait) | **open, named** | 5-10 s `cpu_sys` in the multi-thread mode, dominated by `__psynch_mutexwait` + `__semwait_signal` |
| A genuine 8-thread guest | **still unmeasured** | `-mmt=8` starts 2-3 encoders on a corpus this size (§10.1) |

What is left for threading is therefore not a lock and not the JIT: it is the park/wake
ladder, worth a measured **5-10 s of `cpu_sys`** on a 20 s run when the mode engages, and
the honest 8-thread workload that would show whether the `-mmt=8` penumbra is real.

---

## 11. Closing round: the compile class, re-measured after the frame fix

The dispatcher-entry work landed (`94621e2` + `8b75698`: frame marshalling 32% → 5-6%).
That moved the profile enough that §2–§9's ranking had to be re-derived before anything
else was attempted, so this round measured first. Everything below is at HEAD
`8b75698`, measured with `scripts/real-guest-perf.sh` (deterministic corpus, CRC-checked,
archives on `D:`, per-arm warmup, load-gated) and `/usr/bin/sample` on a
symbol-bearing release build (`CARGO_PROFILE_RELEASE_STRIP=false … _DEBUG=1`).

### 11.1 The matrix after the frame fix

`./scripts/real-guest-perf.sh --classes x,h,mx9 --arms def --reps 3 --max-load 8`
(host load 2.6-4.9 at rep start; the harness's own default gate is 2.0 and would have
skipped every rep, so the limit was raised deliberately and the load is quoted with the
numbers).

| | `x` | `h` | `a -mx9` |
|---|---|---|---|
| wall (median, spread) | **11.767 s** (10.456-13.174) | **3.344 s** (3.301-3.841) | **81.906 s** (76.365-84.275) |
| guest insns | 120.915 M | 5.7915 M | 7.6232 G |
| dispatcher entries (3 reps) | 3.94 / 4.53 / 4.24 M | 4 200 / 4 269 / 4 326 | 68.7 / 82.1 / 79.9 M |
| guest insns / entry | 26.7-30.7 | 1 338-1 379 | 92.9-110.9 |
| compile CPU (all threads, median) | **11.02 s** | **4.97 s** | **27.07 s** |
| guest waits for a compile | 30-32 ms (0.3%) | **299-344 ms (9.3%)** | 0.81-0.84 s (1.0%) |

Three things to read off it. **(1)** `x` costs 11 s of wall to retire 121 M guest
instructions — 11 M insn/s, an eighth of mx9's rate — while burning 8.5-12.3 s of
compile CPU for it. `h` is worse in proportion: 5.8 M instructions, 4.6-5.8 s of
compiling, and 9.3% of its wall is the guest thread *waiting* for compiles. **(2)**
Entry counts are not a stable metric: identical instruction counts ±200, entries ±10%
(§6's correction). **(3)** mx9 is unchanged in wall by any of this: it compiles 27 s of
CPU in the background and pays 1% of wall for it.

These six rows are the state **before** §11.4's change; D2 then moves `x` 11.77 → 6.19 s
and `h` 3.34 → 2.07 s on the same corpus, and leaves mx9 where it is.

### 11.2 Where the guest thread actually is now

Fresh `sample` windows at HEAD, per thread (the guest thread is the one that matters;
`idle/wait` on the four compile workers is 77-83%):

| class | window | guest-thread split |
|---|---|---|
| `a -mx9` | t≈40 s of 82 s | **emitted 86%**, memory helpers 9%, dispatch **4%**, compile ~0% |
| `x` | t≈3-6 s of 11.8 s | **engine-mutex wait 51.6%**, **inline Cranelift 47%**, dispatch 1%, emitted 0% |
| `h` | whole 3.3 s run | **compile 45%**, **engine-mutex wait 40%**, dispatch 14%, emitted ~0% |

The dispatcher is confirmed solved (4-5% on the compute class, 1-14% on the I/O
classes). What replaced it on the I/O classes is not a lock, not dispatch, not the
guest's own code: it is Cranelift. The raw leaves under the `x` guest thread are
`__psynch_mutexwait` (51.6%) plus `regalloc::ion` / `compile_block` / `aarch64
compile_function` internals (47%) — i.e. the guest thread is either compiling or queued
behind someone else's compile for essentially the whole sampled window.

### 11.3 The "tier-0 baseline" is a full Cranelift compile on the guest thread

The code path is the one at the top of `run_compiled`'s miss arm: during boot mode (the
first 5 M guest instructions — which is **86% of `h`'s entire run** and 4% of `x`'s
instructions but most of `x`'s distinct blocks) every first-visited `Pure` block calls
`baseline::try_compile_baseline`. Its body is:

```rust
// crates/wie-cpu/src/jit/baseline.rs
shared.compile_from_kind_shared(fast_api, rip, kind, inv_gen)
```

with a comment claiming "0.2 ms per block vs ~3 ms for Cranelift". It is not: it *is*
Cranelift, at the base tier, holding `lock_unpoisoned(&self.engine)` while it runs, and
the next two lines then enqueue a background tier-1 job for the **same RIP**, which
`compile_from_kind_shared` compiles a second time at the same tier. Measured cost per
compile on the calm-host runs: `compile_us / (bg_compiles + eager)` ≈ **0.7 ms on `x`**
(8.0 s / 11 593) and **1.0 ms on `h`** (4.08 s / 4 079). The 2.4-2.9 ms in an earlier
draft of this section was read off the load-25-40 window, where contention inflates every
compile; it is quoted here as the loaded number, not the per-compile cost.

So the I/O classes currently pay **two Cranelift compiles per block**, one of them
synchronous on the guest thread, and the guest then waits on the engine mutex while the
workers run the copies. That is the whole of §11.2's 85-99%.

### 11.4 Candidate A — fewer compiles: one refutation and one win

**A1 — delete the inline baseline, let the block run iced once and compile on its 2nd
visit (`thr = 1` already means "second visit"). Refuted.** `h`, one binary pair:

```
h base   wall 4.504   compile_us  5 034 026   bg_compiles 2116   deferred    0   cooled_down    4   hot  199
h a1     wall 4.565   compile_us 18 826 134   bg_compiles 4019   deferred 4837   cooled_down 1377   hot 7792
```

Removing the inline compile does not remove work, it removes the *Ready* install — so
every block falls into the `Hot{thr:1}` promotion path, the queue saturates
(`deferred 4837`), each deferral re-promotes later, and the machine compiles **1.9x as
many blocks for 3.7x the compile CPU**. Wall does not move. The baseline path is
load-bearing precisely because it installs the block immediately and never returns to
the promotion machinery.

**D2 — stop compiling the same block twice. Shipped.** Keep the inline baseline (it
installs Ready at the tier the job would choose) and enqueue the background tier-1 job
**only for a block that can actually tier up**. `earns_tier` (`jit/tier.rs:133`) returns
`Speed` only for a self-loop, so `if is_loop { enqueue }` is exactly "when the job would
not be a duplicate". Everything else — bytes installed, Ready timing, ledger writes,
`insert_ready` chain bookkeeping — is unchanged, which the counters confirm:

```
                     bg_compiles        compile_us        iced        eager
h   base                    2139       21 680 863        9132         1940
h   d2                       336        4 633 539        9115         1920
x   base                    5951       49.2-53.7 s      28 577        5642
x   d2                       668        6.0-7.4 s       28 454        5582
```

(`h`/`x` rows are the last pair of each class: `bg_compiles`, `iced` and `eager` are the
in-process counters from `WIE_RUNTIME_PROFILE`, `compile_us` is the per-run spread over
all five `x` pairs. `iced` differs by 0.4% and `eager` by 1%, i.e. within run-to-run
noise; `bg_compiles` and `compile_us` fall by 6-7x, which is the change itself.)

Interleaved pairs, medians of per-pair ratios (arm/base, <1 is better). **Re-measured
on a calm host (load 2.3-2.8)** after the first window ran under load 25-40 while a peer
was building — both windows are reported because they say different things: the calm one
is the number to use, the loaded one shows the win survives contention.

| class | window | per-pair wall ratios | median | compile CPU |
|---|---|---|---|---|
| `h` (5 pairs) | load 2.3-2.8 | 0.820, 0.635, 0.631, 0.629, 0.644 | **0.635** (5/5) | 4.08 → 2.09-2.60 s |
| `x` (5 pairs) | load 2.3-2.8 | 0.602, 0.619, 0.615, 0.618, 0.617 | **0.617** (5/5) | 7.82-8.09 → 3.37-3.63 s |
| `h` (7 pairs) | load 25-40 | 1.036, 0.752, 0.903, 0.903, 0.646, 0.906, 0.769 | 0.903 (6/7) | 21.7 → 4.6 s |
| `x` (5 pairs) | load 25-40 | 1.014, 0.943, 0.868, 0.762, 0.519 | 0.868 (4/5) | ~50 → 6.7 s |
| `mx9s` (3 pairs) | load 13-40 | 1.004, … | 1.00 (flat) | 17.5 → 13.5 s |

**On a calm host this is 1.57x on `h` (3.28 s → 2.07 s) and 1.62x on `x`
(10.04 s → 6.19 s)**, five pairs each, every pair favouring the change, with guest
instructions identical to 8e-6. The loaded window shows the same change compressed to
-10/-13%: under heavy contention the host, not the emulator, sets the wall.

**D3 — boot mode stops lowering the promotion threshold. Shipped.** Distinct from A1
above: the inline baseline and its `Ready` install both **stay**. What went is the
`thr = 1` override that boot mode applied for the first `BOOT_MODE_INSNS` (5 M) guest
instructions. Two different things were pulling blocks into the compiler during boot
and only one of them was earning its keep:

* blocks **≤ 32 insns** (`baseline::is_baseline_eligible`) compile on first sight at
  `pipeline.rs`'s boot arm, *before* the threshold is ever consulted — untouched by
  this change, and load-bearing per A1;
* every **other** block reached the ordinary `Hot { visits, thr }` path, where boot mode
  had set `thr = 1`, i.e. compile on its second visit regardless of size or history.

Removing the override leaves those blocks to earn the compile like any other. Measured
with interleaved pairs (median of per-pair ratios, arm/base, 5/5 pairs, load 2.1-2.2),
one binary, alternate runs:

```
class  arm/base   wall median        compiles     iced     guest insns
h      0.811      1197 -> 969 ms     336 -> 183   9114 -> 10291   5.79 M both arms
x      0.897      3583 -> 3210 ms    668 -> 472   28424 -> 30026  120.9 M both arms
```

**1.23x on `h` and 1.12x on `x`**, every pair favouring the change. The arithmetic that
makes it a win rather than a trade: retired guest instructions are identical and `iced`
rises by 1 177 / 1 602 instructions — **0.02 % / 0.03 % of the run**. The 153 / 196
compiles that disappeared were being executed too rarely to pay for themselves; at
~0.7-1.0 ms each (§11.3) that is 150-200 ms of pure waste on a 1.2 s class.

Two `thr = 1` boot-mode sites remain in `cool_down` / `defer_promotion` (the backoff
paths, reached only after a wait timeout or a full queue, and only once the inline token
bucket is spent). They were unchanged during the measurement above, so the reported win
does not depend on them; they are a separate candidate, and their intent — "do not apply
backoff during warmup" — is not the same as "compile everything immediately".

**`mx9s` is neutral, and that is the prediction, not a disappointment.** Boot mode is
0.27 % of a 1.84 G-instruction run, so the override could only ever have mattered on the
short classes. Cross-commit check (previous commit's binary vs this one, 2 usable
interleaved pairs after discarding a cold first run, load 2.5): paired ratios 1.0042 and
0.9916, **median 0.998**; retired instructions 1.8393 G vs 1.8393 G, and the produced
archive is byte-identical (`crc32 eb3cd188` both arms). Compile counts differ by 0.5 %
(4 801/4 810 vs 4 826/4 835), inside run-to-run noise. A long-running guest eventually
crosses every threshold anyway, which is the same reason the flat-threshold experiment
found nothing on `x`.

**Mechanism confirmed by re-sampling** (`r3_d2`, same windows as §11.2): the `x` guest
thread is still 98% compile-path (72% compiling, 26% mutex wait) and `h` is 65% compile +
14% wait — the *share* is unchanged because the *run* is now about 1.6x shorter. That is
the expected signature of removing a duplicate compile rather than a lock: the same
serial work, done half as often.

**Control:** `long_loop` (the compute micro, whose block is a self-loop and therefore
still takes the tier-up enqueue) measured 0.415/0.427/0.431/0.442/0.433 s base against
0.433/0.439/0.431/0.407/0.432 s d2 — median ratio **1.000**, 1/5 pairs better, i.e. flat
inside the documented 0.25-0.50 s host-dependent spread. The tier-up path is intact.

The win is where the compile class is: **1.57x on `h`, 1.62x on `x`** on a calm host
(and still -10/-13% under load 25-40), and neutral on the compute class, whose guest
thread is 86% emitted code, so removing background compile work cannot help it. The
instruction counts are identical between arms
(5 791 549 vs 5 791 549 on `h`), the iced residue is identical (9 115 vs 9 132), and the
promotions are identical (1 920 vs 1 940) — the change removes compiles and nothing
else. That triple is the mechanism check: if the win had come from executing *less*
guest code or *more* iced, these would have moved.

**A2 (`is_ucrt → (2, false)`, i.e. don't compile a UCRT-ending block on first
execution) was not run**: A1 shows what deferral does to this promotion machine, and A2
defers the same 1 920-5 590 blocks through the same queue. Refuted by the same
mechanism and the same numbers rather than by a fresh experiment.

### 11.5 Candidate B — a real tier-0 emitter: bounded, not built

What it must absorb is now precisely stated: `baseline::try_compile_baseline` must
become an actual single-pass emitter, because today it is a second Cranelift compile.
The requirement set that A1 established empirically:

1. **Install Ready immediately** (A1's 4 837 deferrals / 1 377 cooldown timeouts are
   what happens otherwise).
2. Emit at the tier the block is tagged with, so the register/frame contract at the
   boundary is unchanged (the `JitCtx` ABI, `OFF_*` offsets, `gpr_dirty_bits`).
3. Leave the self-loop tier-up path intact (D2's `is_loop` guard is where that lives).

**Bound (re-derived after D2, §11.4).** D2 removed the duplicate compile, so what
remains is the *single* inline compile per block, and it is now the guest thread's
dominant cost: 72% compiling + 26% engine-mutex wait on `x`, 65% + 14% on `h`
(re-sampled, §11.4). A tier-0 emitter at 0.2-0.3 ms instead of the measured 0.7-1.0 ms would cut that
work by roughly 10x, and if it emits into its own buffer it also removes most of the
mutex wait:

* the compile half (72% of `x`'s 6.19 s) → ~7%, the wait half (26%) → ~0,
* so `x` could go from 6.19 s to ~2.2 s of compile-path time; the remaining wall is the
  guest's own execution, which is small on this class.

**That makes the ceiling ≥1.4x and plausibly ~2x on `x`/`h`** — but it is an upper
bound with a modelled cost, not a measurement, and the cost is the emitted code: a
no-regalloc emitter retires more host instructions per guest instruction, and `x`
retires 121 M guest instructions, so its runtime penalty is not negligible the way it is
for a 5.8 M-instruction `h`. The honest statement is "worth a project, with the runtime
penalty measured on `x` before the emitter is generalised", and it is worth ~0 on `mx9`
(boot mode = the first 5 M of 7.6 G instructions).

**Cost.** A slot-based no-regalloc emitter (every guest GPR op becomes
`ldr ctx+off / op / str ctx+off`) plausibly retires 1.5-3x the host instructions of the
Cranelift block for the same guest instructions. At ~10 ns of extra runtime per execution
of a 30-instruction block (30 × ~1 extra host insn × 0.33 ns), break-even against the
measured ~1 ms compile is **≈100 000 executions** — and the measured population is
nowhere near that: `h` compiles 4 079 blocks for 4 216 dispatcher entries (≈1 entry per
compile, i.e. **essentially every `h` block is first-execution-only**), while `x`
compiles 11 593 blocks for 4.7 M entries (≈400 entries per compile, genuinely hot). So
the emitter wins by a wide margin on `h`-shaped guests and needs the runtime penalty
measured on `x`-shaped ones before it is worth generalising — which is exactly the
"large but not yet unambiguous" verdict. The reason this is *not* implemented here is that
it is an ARM64 emitter plus its own fault/exit paths, not a round-sized change; the
bound above is what a dispatch decision should be made on.

### 11.6 Reconciliation: 225.9 M entries vs the harness's 81.2 M

A peer flagged that §2/§3 projected ~220 M dispatcher entries per mx9 run while
`scripts/real-guest-perf.sh` measures 81.2 M for the same 7.62 G instructions. Both
numbers were real; the doc's row was wrong three times over.

| measurement | insns | entries | insns/entry |
|---|---|---|---|
| §2 table, `2e0a1dd`, `/tmp/corpus`, `C:\out.7z` | 9.370 G | 225.86 M | 41.6 |
| **same binary, same corpus, today, `D:\` archive** | **7.640 G** | **122.62 M** | 62.5 |
| HEAD `8b75698`, calibrated corpus (`D:\`) | 7.623 G | 68.7-82.1 M | 92.9-110.9 |

1. **Different corpus.** §2 used this document's ad-hoc `/tmp/corpus` (random-word
   text); the harness uses its calibrated set (patterned text). Insns differ 7.62 vs
   9.37 G.
2. **The archive hazard, in the direction §1 did not describe.** The §2 runs wrote to
   `C:\out.7z` (the bottle) and `rm`'d only the host path, so the run *updated* a
   pre-existing archive — extra decompress/recompress work. Re-running the identical
   binary on the identical corpus with a `D:` archive gives 7.640 G / 122.6 M, which
   the §2 row does not.
3. **Entry counts are not a stable counter** (§6's correction): ±10% run to run for
   identical instructions, so 68.7 / 79.9 / 82.1 M are the same workload and 81.2 M is
   inside that spread. The harness is right; §2's row and §6's "entries, not wall" aside
   are corrected in place.

### 11.6a The memory-lowering default was the losing arm (measured, fixed)

> The wider audit this came out of — every copy and per-element lock found on
> the present, handler, lowering and SIMD paths, with what landed and what is
> still open — is `docs/architecture/copy-and-lock-audit.md`.

`WIE_JIT_MEM` shipped with **pin** as its default, justified in
`config.rs` by a DOOM Retro startup counter: data-pin IR cut host load-helper
calls 89 % there (21 M → 2.4 M). Helper *count* is not helper *time*, and the
data-pin stage is a third probe (stack → sticky → data pin → helper) emitted
ahead of every memory op, so the whole IR welfare of the block grows to avoid
a walk that `pin_resolve` already collapses.

Two independent paired samples, two binaries, same result:

| class | arm | median ratio | spread | pairs agreeing | insns/entry |
|---|---|---|---|---|---|
| `7za h` | `sticky` vs `pin` | **0.745** | 0.721–0.750 | 6/6 | identical |
| `7za x` | `sticky` vs `pin` | **0.775** | 0.766–0.779 | 6/6 | identical |
| `7za h` (second binary) | `sticky` vs `pin` | **0.759** | 0.745–0.766 | 8/8 | identical |
| `7za x` (second binary) | `sticky` vs `pin` | **0.771** | 0.766–0.776 | 8/8 | identical |

Retired guest instructions match to within the adaptive-spin count, and the
extraction-tree cksum manifest was `3496821080` in all 64 `x` runs in the
larger sample — the work done is the same, in a quarter less time. Compile
cost is higher at *every* block-size bucket (`x`: 119/253/519/720/1741/5205/
17600 µs sticky vs 89/194/375/510/1322/3936/14701 pin at 1/2/4/8/16/32/64
insns), which names the mechanism as IR volume rather than a scheduling
accident.

**Shipped:** `sticky` is now the default; `WIE_JIT_MEM=pin` is the opt-in.
`pin` returns only on *throughput* evidence from the class that motivated it —
a paired run on a GPU-bound guest — not on a helper-call count.

This also reframes row 4 of §11.7 ("the ceiling needs better pin/TLB coverage,
not a faster helper"): better coverage is what `pin` already was, and it cost
22–25 %. The remaining headroom there has to be bought with *cheaper* inline
lowering, not more of it.

### 11.7 The ranking, as it now stands

| # | Lever | Measured cost at HEAD | Ceiling | Effort | Status |
|---|---|---|---|---|---|
| 1 | **Duplicate compile removal** (D2) | `h`/`x` compile CPU 4.08→2.09-2.60 s / 7.82→3.37-3.63 s | **landed: 1.57x `h`, 1.62x `x`** (calm host); flat on mx9 | S | **shipped this round** |
| 2 | **A real tier-0 emitter** (B) | after D2 the remaining inline compile is 72% of `x`'s guest thread (65% of `h`'s) + a further 26%/14% mutex wait; 0.7-1.0 ms per block | **≥1.4x, plausibly ~2x on `x`/`h`**, ~0 on mx9 | XL (ARM64 emitter) | bounded, not built |
| 3 | **Emitted-code quality** | 86% of mx9's guest thread; `opt_speed` measured 1.19-1.21x *slower* | only better *lowering* helps; the optimiser does not | L | open, per-instruction work |
| 4 | **Memory helpers** | 9% of mx9's guest thread; 274 M helper calls/run | ≤9% if every helper access became inline; the inline path currently earns 1.4% (`WIE_JIT_MEM=slow`), so the ceiling needs better pin/TLB coverage, not a faster helper | M | open |
| 5 | Host stops / handlers | ≤1.2% of wall on every class | ≤1.2% | — | dead |
| 6 | Interpreter residue | 0.02-0.9% of retired instructions | ≤0.8% | — | dead |
| 7 | Dispatcher entry | 4-5% (mx9), 1-14% (I/O) | ≤5% | — | **landed (32% → 4%)** |

---

## 12. Where the per-instruction cost goes: the census behind the ns/instruction figure

Date: 2026-11-26. Every number below is a **counter** read out of a profile report
that already existed; no new guest run was taken for this section and no host clock
was read. The one figure that is not a counter — ns per host instruction — is
labelled as arithmetic on a standing number that this section does not re-measure.

The question: a DOOM Retro run retires guest instructions at a rate that works out
to ≈8.4 ns each, and 8.4 ns buys roughly twenty-five host instructions at one per
cycle. So the question is not "where does the time go" in the abstract — it is
**how many host instructions does one guest instruction cost, and which of them can
be removed**. The answer, from this census, is that host instruction *count*
accounts for the time at ~1 IPC: the cost is instructions, not stalls, and every
remaining lever has to delete instructions rather than shorten dependencies. That
reframing is the point of this section.

### 12.1 The two profiles, and why they are kept separate

| profile | phase | guest instructions | frames published | build |
|---|---|---|---|---|
| `/tmp/final.log` | startup / asset load | 14 888 623 046 | 9 | release, tree committed as `7ffc1f6`, `opt_level=none`, `WIE_JIT_REGION` off |
| `/tmp/prof-mirror.log` | render | 34 478 230 275 | 733 | release, a build containing `cced0b6` (the BitBlt stage counters) |

They are never mixed below, and that is deliberate: the per-block census this
section multiplies through (`docs/architecture/scaled-blit-block-attribution.md`,
HEAD `bd9a5c7`) was taken on the **render** workload, and the standing 8.4 ns/insn
figure comes from the **startup** workload. Weighting one phase's counts by the
other's block shares would be the same class of provenance error as quoting a rate
from a run that never reached its frame loop. Where the two are combined here it is
stated, and the combination is the render phase's census with the render phase's
counters.

The builds differ, and neither differs in a way that touches the emit path for the
three attributed blocks: between `cced0b6` and `7ffc1f6` the changes are BitBlt
stage timers (instrumentation in `wie-winapi`) and the removal of the guest-I/O file
mirror (open-time work). That is reasoning about the diff, not a measurement, and it
is the one assumption this section rests on.

### 12.2 The rows that sum exactly to `insns=`

| | startup (`/tmp/final.log`) | render (`/tmp/prof-mirror.log`) |
|---|---|---|
| guest instructions | **14 888 623 046** | **34 478 230 275** |
| … executed from compiled code | 14 822 570 444 (**99.56 %**) | 34 407 172 840 (**99.79 %**) |
| … executed by the iced fallback | 66 052 602 (0.44 %) | 71 057 435 (**0.21 %**) |
| **sum** | **exact** | **exact** |

The interpreter residue is a rounding error on this workload, which is consistent
with §5's dead-end verdict and is worth keeping next to any future proposal to
optimise the residue.

### 12.3 The event rows, as one event per N guest instructions

| count | startup | render |
|---|---|---|
| dispatcher re-entries (`cache_hits`) | 127 786 895 → 1 per **116.5** | 129 374 235 → 1 per **266.5** |
| native chain hops | 955 156 362 → 1 per **15.6** | 988 754 828 → 1 per **34.9** |
| chain GPR stores | 6 093 305 170 → **6.38 per hop** | 6 302 102 268 → **6.37 per hop** |
| memory-**helper** calls (`load`+`store`) | 1 210 033 → 1 per **12 304** | 19 548 149 → 1 per **1 764** |
| API stops | 80 802 → 1 per **184 261** | 206 590 → 1 per **166 892** |
| blocks compiled | 1 291 | 1 810 |

Every dispatch-side row is small: one hop per 35 guest instructions in the render
phase, six register stores per hop, one helper call per 1 764 instructions. None of
them can be most of a 25-instruction bill. The cost is in what one block iteration
does per guest instruction, which is §12.4.

### 12.4 Prologue and epilogue, amortised: the 7.47 divisor

The census in `scaled-blit-block-attribution.md` gives a **per-iteration** bill and
a **per-entry / per-exit** bill. A per-iteration figure alone therefore understates
the true cost, and the counters say exactly how much the entry/exit bill is spread
over:

```
hops / dispatcher re-entries = 955 156 362 / 127 786 895 = 7.47 iterations per re-entry
```

| block | per iteration | + (entry 62 + exit 275) / 7.47 | loop-only | **amortised** | share of leaf samples |
|---|---|---|---|---|---|
| `0x1800d8fb0` | 174 | 174 + 45.1 = 219.1 | 21.75 | **27.39** | 54.1 % |
| `0x1800a1c60` | 115 | 115 + 45.1 = 160.1 | 19.17 | **26.68** | 27.4 % |
| `0x1800cef0b` | 177 | 177 + 45.1 = 222.1 | 17.70 | **22.21** | 14.6 % |
| **weighted over 96.1 %** | | | 19.60 | **25.37** | |

So one guest instruction costs ≈**25 host instructions**, not ≈20: the prologue
(62) and epilogue (275) are **45 host instructions per iteration** — 21 % of block
1's 219.1 — and larger than the 20-per-iteration probe guard. A per-iteration
census cannot see that term at all, which is why the divisor is written out here
rather than left implied.

### 12.5 The consistency check — arithmetic only

Standing figure, **not re-measured here**: ≈8.4 ns per guest instruction (derived
elsewhere from 125.3 s of emulation and 14.9 G instructions).

```
8.4 ns / 25.37 host instructions per guest instruction = 0.331 ns per host instruction
                                                      = 3.02 host instructions per ns
                                                      = 0.94 host instructions per cycle at 3.2 GHz
```

That is the load-bearing result: the host instruction count **accounts for** the
observed time at roughly one instruction per cycle. There is no large unexplained
residue, so the levers in this document are correctly modelled as "delete
instructions"; a story about stalls, caches or branch misses has to beat this
arithmetic first, and nothing measured so far does. [Arithmetic on a standing
figure — no clock was read for this line.]

### 12.6 Three things that are not load-insensitive, stated as gaps

1. **Inline probe-guard executions have no counter.** The fast path re-checks the
   mapping guard inline; only probe *misses* reach a helper, which is why the helper
   rows in §12.3 are near zero. Any statement about guard cost per instruction comes
   from the static census (15–16 host instructions per memory operand; 3 per access
   and 6 per iteration on the current blit iteration; 20 per iteration per this
   census), never from a runtime count. If a future change wants the *dynamic*
   operand count, it needs a counter that does not exist yet.
2. **The iced fallback's per-instruction cost is unquantified.** It is 0.21 % of
   render-phase instructions and there is no counter for what one of them costs. At
   any plausible 10x a compiled instruction it is ~2 % of host work; that is an
   order-of-magnitude bound, not a measurement, and no lever should rest on it.
3. **Rate is a phase property.** These two profiles retired 14.9 G and 34.5 G guest
   instructions over runs of similar length, so a single ns-per-instruction figure
   describes the phase mix, not the JIT. The 8.4 ns figure above is therefore the
   startup phase's, and this section quotes no other rate.

### 12.7 The lever, attributed the way the campaign has settled it

**The flag construction: at the settled ≈21 % of emitted code and ≈37 % per
iteration of the scaled-blit block it costs 64.4 of that block's 219.1 host
instructions per iteration, which weighted by the block's 54.1 % of leaf samples is
≈4.4 of the 25.4 host instructions each guest instruction costs (17 %); at the
narrower provably-dead figure (39 of 174) it is ≈2.6 per guest instruction (10 %).**

Its nearest rival is the probe guard at 20 host instructions per iteration, and the
term this section adds to that ranking is the prologue/epilogue pair at 45 per
iteration — 21 % of the bill, larger than either. All three are counts of
instructions that exist in emitted code today; none needs a new measurement to be
compared.

### 12.8 What this section does not do

It does not re-derive the flag census (that closed at ≈21 % of emitted code and ≈37 %
per iteration of the blit block), it does not optimise anything, and it assigns no
work: the census is handed to the campaign owner, who owns the ordering. The
reproduction path is the two profile logs named in §12.1 plus the census document
named in §12.4; nothing here needs a run.
