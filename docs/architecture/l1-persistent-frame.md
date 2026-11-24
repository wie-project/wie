# L1 — one persistent `JitCtx` per engine

Date: 2026-11-24. Branch `feat/dll-coverage`. Implements lever **L1** of
[`performance-ceiling.md`](performance-ceiling.md) §4.1: stop building and
tearing down the compiled-code frame on every dispatcher entry.

**Result: 1.33x wall / 1.31x CPU on `7za a -mx9`** (median of 12 interleaved
A/B pairs; 11 of 12 favour the change), with the guest's output byte-identical
and `long_loop` unchanged. The `_platform_memmove` that the ceiling analysis
identified (8.9% of the guest thread in its sampling window; **31.4%** in mine,
same method) is **0.9%** afterwards — the copy is gone, not merely cheaper. Nothing else in the frame's ABI moved: the emitted
offsets are unchanged, so `CODE_FORMAT_VERSION` stays at **3** (see §3).

---

## 1. What the frame was, and what it is now

`JitCtx` (`crates/wie-cpu/src/jit/lower/mod.rs`) is the `#[repr(C)]` struct that
compiled code receives as `*mut JitCtx`. Its layout is the ABI: emitted IR
loads `rflags`, `rip`, `xmm`, `pins`, the sticky/hot TLB slot, `edge_ic_*`,
`shadow_sp`/`shadow_ret`, `chain_depth`, `inv_gen_*` and the per-run counters
through `OFF_*` constants.

Before this change `run_compiled` built one **per dispatcher entry**, by value:

* copy in — `tlb` (2304 B), `pins` (`8 × 40 B`), `shadow_ret` (256 B),
  `gpr` (128 B), `xmm` (256 B) = **≈3.2 KB**, plus ~50 scalar fields;
* run the block;
* copy out — the same bulk fields back into `self.thread`, plus `gpr`/`xmm`/
  `rflags`/`rip` into `RegFile`.

On the calibrated `mx9` workload that is ≈200 M entries per run, so the struct
copy alone was 1.2 TB of memory traffic. It was visible as `_platform_memmove`
under `finish_compiled` in `sample`, which is how the ceiling analysis found it.

Now there is **one frame per engine thread**, allocated once in
`PerThreadJitState::new()` (`JitCtx::new_persistent`) and passed by pointer:

```rust
let ctx = &mut self.thread.ctx;          // the frame, not a copy of it
let regs = &mut self.thread.regs;
… stamp entry state …
unsafe { (meta.func)(std::ptr::from_mut(ctx)) };
```

The bulk fields are not copied anywhere: `tlb`, `pins`, `sticky_*`,
`edge_ic_*`, `shadow_sp`/`shadow_ret` and `tlb_hot_*` now live **only** in the
frame. The fields that were duplicated in `PerThreadJitState` and copied back
after every run are gone from that struct — that is where the 3.2 KB of state
moved to, so the engine's footprint is unchanged; what disappears is the
per-entry round trip (and `size_of::<JitCtx>()` = **3744 B** of guest-thread
stack per entry, which also buys headroom on the 48-deep chain path).

Per entry the dispatcher now writes only what the guest can have changed since
the last entry, and resets only what accumulates during a run:

| written every entry | reset every entry (`must`, frame outlives a run) | left alone (`must survive`) |
|---|---|---|
| `gpr[16]`, `xmm[16]`, `rflags`, `rip`, `gs_base` | `fault`, `fault_addr/size/access` | `tlb`, `tlb_hot_*` |
| `mem`, `chain_slots`, `large_free`, `inv_gen_ptr` | `gpr_dirty_bits`, `xmm_dirty_bits` | `pins`, `sticky_*` |
| `mem_gen`, `inv_gen_baked`, heap layout (3) | `chain_depth`, `insn_acc` | `edge_ic_va/fn/rr` |
| | `chain_hops`, `chain_store_ops` | `shadow_sp`, `shadow_ret` |
| | `load_calls`, `store_calls`, `mem_path` | |

Two properties make that table the whole safety argument, and both are stated
on the struct:

1. **Every raw pointer in the frame is re-stamped at each entry** (`mem`,
   `chain_slots`, `large_free`, `inv_gen_ptr`). A persistent frame that stored
   an address of its own owner would be invalid the moment the engine moved;
   re-stamping per entry is what makes moving harmless. `new_persistent` starts
   them all null so a missed stamp faults loudly instead of reading a stale
   pointer.
2. **Nothing stores a frame pointer across entries.** Chain edges and micro-stubs
   only ever receive the pointer the dispatcher just handed the block, and
   `chain_slots` stores block fn pointers, never `*mut JitCtx`. This is what
   keeps "one frame" from becoming "one frame reached from two places".

## 2. Why this shape and not `&mut` fields in the frame

The other shape the analysis named — keep the frame a temporary but turn `tlb`,
`pins` and `shadow_ret` into references to stable storage — was rejected on
three concrete grounds, not on taste:

* It **changes the layout**, so every `OFF_TLB_HOT_*`, `OFF_PINS`,
  `OFF_STICKY_*` and `OFF_EDGE_IC_*` access in emitted IR gains a load, all
  persisted blobs must be invalidated (`CODE_FORMAT_VERSION` 3 → 4), and the
  `const _` offset assertions have to be rewritten. This shape changes *no*
  offset, so persisted code keeps working (§3).
* It puts a lifetime parameter on `JitCtx`, which then has to be threaded
  through `CompiledBlock::func`, `CompiledRunMeta`, `trampolines::ctx_mut`, the
  `wie_ucrt_*` import symbols and ~20 test call sites — i.e. exactly the
  lifetime argument the ground rules say not to introduce unless it can be
  fully stated.
* It buys nothing this shape does not already buy: both remove the copy.

Owning the frame also inherits an existing, already-audited decision instead of
making a new one: `PerThreadJitState` is `Send` (one owner thread) and
deliberately **not** `Sync`, so the `&mut` the dispatcher holds cannot be
observed by another thread. No new `unsafe impl Send` was needed and none was
added; the existing site's SAFETY comment was extended to cover the frame's
pointers, and `ctx_mut`'s contract was rewritten to say what now keeps the
frame alive (the engine's ownership, not the native frame).

**No new `unsafe` site was introduced.** `run_compiled`'s existing
`#[allow(unsafe_code)]` block covers the call; the field stamps are safe Rust
and the borrow checker proves the disjointness (`ctx`, `regs`, `stats`,
`shared` are separate fields). No `#[expect(unsafe_code)]` was added anywhere.
What *did* change is the text of every SAFETY comment that said "owned by
`run_compiled` for the native frame" — that claim is no longer true, and a
stale SAFETY comment is worse than none.

## 3. `CODE_FORMAT_VERSION` — not bumped, and why that is correct here

`emit_fingerprint` hashes config knobs and build shape, not emitter layout, so
a layout change without a version bump would replay blobs that read the wrong
offsets. **This change does not alter `JitCtx`'s layout**: same fields, same
order, same `repr(C)` packing, same `OFF_*` values — the `const _` block in
`lower/mod.rs` that asserts every offset still passes unchanged, and it is
evaluated at compile time. Emitted code cannot tell the difference, so bumping
`CODE_FORMAT_VERSION` (currently **3** in
`crates/wie-cpu/src/jit/engine/code_cache.rs`) would only throw away valid
persisted blocks. **It stays at 3.**

The rule still binds the next person: if you add, remove, reorder or retype any
`JitCtx` field, bump `CODE_FORMAT_VERSION`, because the offset assertions catch
*reordering of the fields listed there* but not a new field inserted before
them that the assertions happen to still agree with.

## 4. The invariant, and how it is verified

> A compiled block must observe exactly the same `JitCtx` whether it was entered
> from the dispatcher or from a chain edge.

With a temporary frame that held because both paths received the same pointer
for one run. With a persistent frame it holds **structurally** — there is one
frame and both paths are handed `&mut` to it — which is a strengthening. The two
ways to break it are (a) a future patch that reintroduces a second copy or a
dispatcher-only prelude, and (b) resetting too much per entry, which silently
turns chaining back into cold starts.

Both directions are now tested, not assumed:
`crates/wie-cpu/src/jit/tests/frame_entry_tests.rs`
(`chain_edge_and_dispatcher_entry_observe_the_same_frame`).

* One fixture, two entry paths. Block `A` is `mov rax,0 ; jmp B`; `B` loads an
  `xmm` with `movdqu [r15]`, stores it back out (`movdqu [r15+0x10],xmm3`), does
  GPR arithmetic, `call`s a third block and `ret`s through it, then jumps to a
  stop address. `B` is entered **once via the chain edge from `A`** and **once
  directly from the dispatcher**, four times in total including two arms taken
  after deliberately poisoning the frame's per-run fields.
* Equality is asserted on **all 16 GPRs, `rflags`, `rip`, all 16 XMM slots, and
  `shadow_sp` + the whole `shadow_ret` array** — not just the GPRs. The shadow
  stack is seeded to depth 5 with a sentinel in an untouched slot and a
  sentinel-bearing slot the `call` overwrites, so both "did not persist" and
  "cleared by mistake" fail.
* A **positive control** proves the arm really took a chain edge: the chained
  arm must take exactly one more `chain.hops` than the dispatched arm (measured
  3 vs 2). Without it the equality could pass while `A` reached `B` through the
  dispatcher and proved nothing.
* **Teeth were checked by mutation**, not by inspection:
  deleting `ctx.fault = 0` makes the poisoned arms fail with
  `arm at 0x10b00020 ended with InvalidMemory(…) after 0 steps (a leftover
  fault in the persistent frame?)`; adding `ctx.shadow_sp = 0` fails with
  `the call/ret pair must leave the seeded shadow depth intact — a reset here
  means the dispatcher is clearing state it must preserve`. Both mutations were
  reverted.

## 5. Measurement

**Method** (unchanged from the previous three rounds): two release binaries from
one base, interleaved pairs `A,B,A,B,…`, ratio per pair, median of ratios.
`CARGO_PROFILE_RELEASE_STRIP=false CARGO_PROFILE_RELEASE_DEBUG=1`, so `sample`
resolves symbols. Workload: `7za a -mx9 -y D:\out9.7z D:\rnd D:\txt D:\mid`
over a 12 MB corpus (`rnd/` 2×4 MB incompressible, `txt/` 2 MB compressible,
`mid/` 2 MB), archive written to `D:` so it is `rm`-ed between runs (the
bottle-archive hazard in §1 of the ceiling doc) — one run is 96–180 s
depending on how loaded the shared host is.

A checked-in harness now exists for reproducing this —
`scripts/real-guest-perf.sh --classes mx9 --reps 3` with `CLI=` pointing at one
binary and `WIE_PERF_*` describing the other (it also refuses a corpus whose
CRCs have drifted and skips reps under load). The numbers below were taken with
the ad-hoc equivalent, before that script landed, and the classification of
hazards agrees: archive on `D:`, ratios only, corpus pinned.

Two additions to the method, both because this host's absolute times moved
**96 s → 180 s for the same binary** inside one session:

* `cpu_user_ms + cpu_sys_ms` is recorded beside wall and used as the primary
  metric. On this box wall is contaminated by other tenants: one patch arm
  burned 123 s of CPU over 147 s of wall at load average 19.6, and its extra
  `compiles` (363 vs 150) are the guest being starved, not the patch.
* A three-pair confirmation round was run on a **freshly built pair from the
  current tree**, after a peer landed a `jit/block.rs` stack-base analysis fix
  mid-session, so the headline number is not the artefact of a stale base.

**Round 1+2** (base = tree at 03:05; rows 1–5 are round 1, rows 6–9 round 2, one
warm pair discarded before each):

| pair | base wall | patch wall | wall ratio | base cpu | patch cpu | cpu ratio | guest insns (base / patch) |
|---|---|---|---|---|---|---|---|
| 1 | 96.8 | 71.6 | 1.3520 | 99.7 | 74.8 | 1.3318 | 12428663580 / 12428654516 |
| 2 | 96.2 | 83.5 | 1.1520 | 99.5 | 80.9 | 1.2295 | 12428663120 / 12428657013 |
| 3 | 124.3 | 73.3 | 1.6963 | 123.2 | 76.4 | 1.6122 | 12428658662 / 12428654118 |
| 4 | 122.7 | 149.6 | 0.8198 | 123.3 | 141.1 | 0.8739 | 12428660859 / 12428654835 |
| 5 | 163.5 | 123.7 | 1.3214 | 168.7 | 129.6 | 1.3017 | 12428666788 / 12428658502 |
| 6 | 163.7 | 124.2 | 1.3179 | 169.1 | 129.5 | 1.3057 | 12428668559 / 12428657138 |
| 7 | 162.6 | 122.8 | 1.3244 | 167.8 | 129.7 | 1.2943 | 12428667785 / 12427996969 |
| 8 | 179.6 | 125.4 | 1.4318 | 171.5 | 130.9 | 1.3104 | 12428667222 / 12428655223 |
| 9 | 165.2 | 121.2 | 1.3623 | 170.4 | 126.6 | 1.3452 | 12428669555 / 12428660642 |
| **median** | | | **1.3244** | | | **1.3057** | identical to 7 significant digits |

Pairs 3 and 4 are the two outliers in opposite directions and they cancel: pair
3's base arm ran with 164.7 M entries while its patch arm had 235.9 M (the
compiler fell behind a starved guest), pair 4's patch arm is the load-19.6 arm
described above.

**Round 3** (fresh binaries from the current tree, 3 pairs):

| pair | base wall / cpu | patch wall / cpu | wall ratio | cpu ratio | entries (M) |
|---|---|---|---|---|---|
| 1 | 162.1 / 167.3 | 123.7 / 128.9 | 1.3102 | 1.2973 | 197.5 / 211.1 |
| 2 | 164.8 / 170.1 | 123.4 / 128.7 | 1.3355 | 1.3213 | 202.2 / 199.5 |
| 3 | 164.3 / 169.6 | 122.3 / 127.6 | 1.3435 | 1.3296 | 200.8 / 190.8 |
| **median** | | | **1.3355** | **1.3213** | |

**All 12 pairs: median ratio 1.330 wall / 1.308 CPU, 11/12 favouring the patch.**
Ratio of medians over all pairs: 1.33 wall.

Correctness of the output, which is the primary evidence that nothing moved:
every one of the ~24 runs produced an archive of **4 423 813 bytes,
SHA-256 prefix `bcbc277e34db8725`** — identical in both arms and in every run
either arm. Guest instructions retired are identical to within 7×10⁻⁷
(`12 428 6xx xxx`); the residual is compile-timing, exactly as the ceiling doc
predicts. Dispatcher-entry counts (`cache_hits`) are **not** stable across arms
(165–254 M) — they move with background-compile timing — which is why the
insn count, not the entry count, is the invariant to compare.

**Mechanism, re-sampled** (`/usr/bin/sample`, 5 s mid-run, 1 ms interval; both
arms sampled at the same point of the same workload):

* base: `_platform_memmove` under the main (guest) thread = **763 of 2430
  samples = 31.4%**, and its five largest call sites are all inside
  `JitCpu::finish_compiled` (offsets +5940, +6412, +7196, +6684, +6120).
* patch: **22 of 2390 samples = 0.92%**, and **none** of them is under
  `finish_compiled` any more — the residue is guest-memory helper traffic
  (`wie_jit_load`).

So the copy did not shrink, it disappeared, and the profile attributes the same
place the code change did. (The 31.4% figure is higher than the 8.9% in the
ceiling doc because it is a 5 s window inside the incompressible-data phase,
where entries are densest; the collapse from 763 to 22 samples under identical
conditions is the measurement that matters.)

**Compute did not regress** — `micro-exes/out/long_loop.exe`, 7 interleaved
pairs on `emu_ms`:

```
base  [367.42 375.05 379.75 377.51 373.86 376.48 366.07]  median 375.05
patch [375.82 369.10 376.98 374.06 375.59 375.26 379.97]  median 375.59
median of per-pair ratios 1.0033   ratio of medians 0.9986
insns 1 100 000 012 in every run, of either arm; cache_hits = 1 in every run
```

`cache_hits = 1` is the point: the whole 1.1 G-instruction loop runs from **one**
dispatcher entry, so this fixture is structurally blind to entry cost (the
ceiling doc says so, and it is why it is the right control rather than the right
benchmark).

## 6. Gates

All run on the current tree (which carries peer edits in `jit/block.rs`,
`jit/lower/**` and `wie-runtime/{memory,quantum,session}`, not only mine).

| gate | result |
|---|---|
| `cargo nextest run --workspace` | **2033/2033 passed**, 0 skipped (182 s) |
| `cargo nextest run -p wie-cpu` | 348 → 353 passed, includes the new frame test |
| `cargo clippy --workspace --all-targets` | **0 warnings** |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo fmt --all -- --check` | clean |
| `scripts/check.sh --only file-sizes --only dep-direction --only winapi-port --only state-bindings` | all pass; `whole-state bindings ok (439 = budget 439)` |
| `scripts/run-micro-suite.sh all --short` (patched release binary) | **78 fixtures (65 run, 13 skipped by the suite's own `SKIP_EXES`), `micro-suite (all): ok`** — includes `cpu_fp`, `crypt_hash`, `rep_lengths`, `gdi_alpha`, `gl_quad`, `snake`, `chain_hops` |
| `WIE_CPU=iced` on a real guest (`7za h D:\mid\m0.bin`) | `CRC32 DDCA0EF5` / `Everything is Ok`, identical to both JIT arms |
| notepad, 1.5 MB / 23k-line CRLF file, `--screenshot --max-api 200000` | exit 0, `insns=1900 iced=45 cache_hits=58` in both arms, captured BMPs **byte-identical** (`sha256 18fea754…`) |
| Doom Retro (bottled guest), `--screenshot`, 130 s cap | both arms reach ~21.4 M guest instructions with **identical host memory-helper counts** (`load=1 056 407`, `store=7 205`) |

The 23 000 CRLF pairs and the Doom `xmm`/`movdqu` shape are the two historical
frame bugs the ceiling doc names. What is verifiable headlessly today is covered
above; see §7 for what is not.

## 7. What I could not verify, and the risks I would insist on

* **The full notepad file load is not reachable headlessly.** `--screenshot`
  stops at the first rendered frame (1 900 instructions, before the document is
  parsed), and `--persistent` rejects guest argv, so `notepad.exe C:\big.txt`
  cannot be driven to the CRLF expansion without a window server. What covers
  that path instead: the new test's `xmm` identity across the chain edge, the
  Doom-shaped fixture in `chain_tests`, `simd_parity_tests`, and the 78-exe
  micro suite. **A GUI class run on a machine with a window server is still the
  missing check**, and it was missing before this change too.
* **Doom Retro never gets a frame** in the headless path, so its *rendering* is
  not compared — only its instruction stream and memory-helper counts.
* **Host noise is larger than the effect in any single pair.** The honest
  statement is the median over 12 pairs (1.33) and the mechanism (31.4% → 0.9%),
  not any individual run. Anyone re-measuring on a busy machine should expect
  ±30% per pair and should not trust a 3-pair result from one session — my first
  3-pair round said 1.116 and a later 5-pair round of the same binaries said
  1.302.
* **The peer's `jit/block.rs` stack-base fix landed mid-session.** The 9-pair
  round predates it; round 3 (3 pairs, 1.3355) is on the current tree and agrees,
  so the conclusion is not base-specific. But strictly, only round 3 measures
  the code as it is now.
* **The frame is 3.7 KB (3744 B) of per-thread state that no longer sits on the stack.**
  Each engine now holds it permanently. That is a wash against the 3.2 KB it
  replaced, but a future multi-engine-per-thread design (nested dispatcher
  re-entry, e.g. a host callback that runs guest code) would alias one frame.
  Nothing re-enters `run_compiled` today — I checked every caller: `step_one`
  and the hot-chain loop in `cpu_engine.rs` are the only entries, and no host
  helper calls back into the dispatcher. **If a re-entrant path is ever added,
  the frame must become a stack of frames.**

## 8. What is left in the entry path

The change removed the copies; it did not remove the entry. What a dispatcher
entry still does on `mx9`, in the order the code does it:

1. `self.shared.mem_gen.load(Acquire)` and the `pins_gen` compare;
2. the **pin-shape diagnostic block** — a loop over 8 pins computing
   `pin_stack_bytes` / `pin_heap_bytes` / `pin_allow_bits` for `WIE_RUNTIME_PROFILE`,
   recomputed on every entry although `pins` only changes when `mem_gen` does
   (measured previously at noise level on its own, +1.0% median, so it was left
   alone here to keep this round single-mechanism);
3. `read_unpoisoned(&self.shared.mem)` — a `std::sync::RwLock` **read guard taken
   per entry** just to obtain `mem`, which is a process-lifetime `Arc`;
4. the 16-GPR + 16-XMM snapshot (~384 B) and ~20 scalar stamps;
5. the indirect call;
6. the writeback (16 GPR + 16 XMM + `rflags` + `rip`) and ~15 counter folds.

(3) is the next thing I would measure: it is an unconditional lock acquire on the
hottest path in the emulator, and the pointer it protects cannot change while
the session runs. `sample` cannot separate it because it inlines into
`finish_compiled`'s self time — it needs a dedicated A/B, not a profile.

> **Done, next round: 3.47 ns/entry.** The lock is gone (the pointer is captured
> once per engine and stamped into the frame like the other raw pointers), worth
> ~0.6% of `7za a -mx9` — below this host's per-pair resolution, which is why it
> was measured on an entry-dominated guest. The same round attributed the rest of
> the entry: [`dispatcher-entry-attribution.md`](dispatcher-entry-attribution.md).

Beyond that, L1 does not stack with anything cheap: the ceiling doc's L7
(region formation) is what multiplies this win, and the entry count
(190–210 M on this corpus) is now the whole remaining cost of the dispatcher.
