# JIT code memory on Darwin — what the OS allows, what it costs, and why we left it alone

Date: 2026-11-24. Host: Apple M3, macOS 26.5.2 (25F84), arm64, `hw.pagesize` =
**16384**, `hw.l1icachesize` = 131072, `hw.l2cachesize` = 4194304,
`cachelinesize` = 128. Guest: DOOM Retro 6.3 under the `doomretro` bottle,
`--screenshot` (headless GUI) entry, 100 s cap.
Measurement shape: **one run per arm**, on a host carrying other work
(load average 3.7–5.7 on 8 cores). Absolute wall/frame numbers are therefore
*not* results; every conclusion below rests either on a microbenchmark run
in isolation or on a counter that is load-independent.

This document answers five questions that all reduce to "should we touch JIT
code memory": (1) can it be huge-paged or `MAP_JIT`-backed on this OS; (2) how
much of a real run is the code-memory path; (3) is W^X toggling flushing the
i-cache per block; (4) does prefetching a restored cache pay; (5) is code
locality a bottleneck.

**Result: no, to all five, and the numbers are large margins rather than close
calls.** The entire per-block code-memory path — one `mmap`, first touch, one
`mprotect(RX)`, one 16 KiB `ic ivau` flush — is **2.8–4.1 µs per block = 36–53 ms
of a 100 s run (0.04 %)**, against a **~2–4 ms** compile. Darwin offers no
huge-page mechanism to a user process at all. W^X is already batched correctly
and only needs to be because code must be executable before its first chained
entry. Prefetch cannot pay on a 1.2 µs flush. Code locality is bounded at
≲3 % of JIT time by a structure fact (a block's inner loop stays inside the
block), even though the *layout* has a genuine L1I aliasing defect worth
recording.

Two knobs were implemented, measured, and then **deleted** rather than shipped
default-off; §9 records them so nobody rebuilds them.

---

## 1. What the OS allows (measured, not assumed)

Probe program, compiled with `clang -O2`, run on the same host:

```c
printf("hw.pagesize=%ld vm_page_size=%lu\n", sysconf(_SC_PAGESIZE), vm_page_size);
mmap(NULL, 16384, PROT_READ|PROT_WRITE|PROT_EXEC, MAP_ANON|MAP_PRIVATE, -1, 0);          // W+X, no MAP_JIT
mmap(NULL, 16384, PROT_READ|PROT_WRITE|PROT_EXEC, MAP_ANON|MAP_PRIVATE|MAP_JIT, -1, 0); // W+X + MAP_JIT
madvise(p, 4<<20, 14);                                                                  // Linux MADV_HUGEPAGE
mach_vm_allocate(mach_task_self(), &a, 32<<20, VM_FLAGS_ANYWHERE|VM_FLAGS_SUPERPAGE_SIZE_2MB);
```

Verbatim output:

```
hw.pagesize=16384  vm_page_size=16384
mmap RWX (W+X at once, no MAP_JIT)             DENIED  errno=13 (Permission denied)
mmap RWX + MAP_JIT                             OK      0x104de4000
mmap RW + MAP_JIT                              OK      0x104de4000
  MADV_NORMAL                                    ret=0
  MADV_WILLNEED                                  ret=0
  MADV_DONTNEED                                  ret=0
  MADV_FREE                                      ret=0
  madvise(14) [Linux MADV_HUGEPAGE value]        ret=-1 errno=22 (Invalid argument)
  madvise(12) [Linux MADV_DONTFORK value]        ret=-1 errno=22 (Invalid argument)
  mach_vm_allocate(2MB) kr=4 FAIL addr=0x0 align2M=-
  mach_vm_allocate(SIZE_ANY) kr=4 FAIL addr=0x0 align2M=-
```

Read off that table:

- **16 KiB pages are real and are the allocation granularity.** Every mapping is
  a multiple of 16 KiB and every `mmap` base is 16 KiB-aligned. A 40-byte block
  costs a 16 KiB page.
- **There is no huge-page mechanism for a user process.** `MADV_HUGEPAGE` does
  not exist on Darwin (`sys/mman.h` stops at `MADV_ZERO`), and passing the Linux
  value 14 is `EINVAL` — not "accepted but ignored", rejected. The only
  superpage API in the SDK is `VM_FLAGS_SUPERPAGE_{SIZE_ANY,SIZE_2MB}` in
  `mach/vm_statistics.h`, and `mach_vm_allocate` with either returns
  `KERN_INVALID_ARGUMENT` (4): superpages are not reachable from user space on
  this OS, entitlement or not. **`VM_FLUSH_RESET_PERMS` does not exist on Darwin
  at all** — it is a Linux kernel-internal `vm_flag` used by `module_alloc`
  paths (`text_poke`, BPF), never a user ABI — so there is nothing to translate.
- **W+X at the same address is refused without `MAP_JIT`** (`EPERM`), which is
  the arm64 default and not something this binary opted into.
- **`MAP_JIT` is accepted here, but only because of the signature.**
  `codesign -dv target/release/wie` reports `flags=0x20002(adhoc,linker-signed)`,
  `Signature=adhoc`, no entitlements. A hardened-runtime signature
  (`--options runtime`) would refuse `MAP_JIT` without
  `com.apple.security.cs.allow-jit`. Any design that *requires* that entitlement
  is out of scope for this project: it would stop `cargo build` output from
  running.
- **`MAP_JIT` is not a drop-in `RWX`.** It arms the arm64 JIT write-protect, so
  the region is not writable until the writing thread calls
  `pthread_jit_write_protect_np(false)`. Empirical: the first version of the
  layout benchmark below wrote its stubs straight into a `MAP_JIT` mapping and
  died with **SIGBUS**; `pthread_jit_write_protect_supported_np()` returns 1 and
  the calls fixed it. That state is **per thread**, which is the reason it does
  not fit this JIT (§4).

## 2. What the code-memory path actually does today

Traced through cranelift-jit 0.133.1, because the cost only makes sense as the
exact sequence:

1. `JITBuilder` is built without a memory provider, so `JITModule::new` installs
   `SystemMemoryProvider` (`backend.rs`: `builder.memory.unwrap_or_else(||
   Box::new(SystemMemoryProvider::new()))`).
2. Each block define calls `CompiledBlob::new(…, JITMemoryKind::Executable)` →
   `Memory::allocate(size, align)` → `MmapMut::map_anon(page_ceil(size))` when
   the current buffer has no room. There is a `// TODO: Allocate more at a time`
   at that site.
3. `lower::compile_block` calls `finalize_definitions()` **once per block**, and
   `Memory::set_readable_and_executable` begins with `finish_current()`, which
   *replaces* the current buffer with an empty one. So the buffer never gets
   reused: **every block gets its own `mmap`, always 16 KiB because
   `map_anon` rounds up.**
4. That call then, per newly-protected allocation:
   `wasmtime_jit_icache_coherence::clear_cache(ptr, len)` — an `ic ivau` per
   64 bytes plus `dsb ish` and `isb` — then `region::protect(ptr, len,
   PROT_READ|PROT_EXEC)`, then a single `pipeline_flush_mt()`. On aarch64 macOS
   that last call is the **no-op arm** of `wasmtime-internal-jit-icache-coherence`
   (`details::pipeline_flush_mt` returns `Ok(())` for anything that is not
   Linux-aarch64); it is *not* a whole-process flush.

So per block: 1 `mmap` + page faults + 1 `mprotect(RX)` + ~256 `ic ivau` + 2
barriers. Nothing else. `already_protected` in `Memory` is a cursor over
`allocations`, so a page is mprotected and flushed **exactly once, ever** — see
§4.

## 3. What that path costs, per block and per run

Sequenced exactly like the JIT does it (16 KiB `mmap`, write 1 KiB of code,
`mprotect(RX)`, the same `ic ivau`/`dsb`/`isb` loop), 12 808 iterations — the
block count of the 100 s run — twice on the same loaded host:

| step | run 1 | run 2 |
|---|---|---|
| `mmap` 16 KiB | 0.39 µs | 0.29 µs |
| first-touch write 1 KiB | 0.02 µs | 0.02 µs |
| `mprotect` RW→RX (single, derived from a 2-transition round trip) | ~0.49 µs | ~0.39 µs |
| `ic ivau` × 256 + `dsb ish` + `isb` over 16 KiB | 1.19 µs | 1.12 µs |
| **whole path, per block** | **4.11 µs** | **2.79 µs** |
| **× 12 808 blocks** | **52.7 ms** | **35.7 ms** |

Against a **2–4 ms compile per block** (`compile_us` 21.4–40.5 s over ~8.5–9.4 k
compiles in the runs below). Code memory is ~**1 000× cheaper than the thing it
wraps**: 36–53 ms of a 100 s run, **0.04 %**. Note the i-cache flush, 1.1–1.2 µs,
is the largest single item and it is *required correctness work* on a
non-coherent I/D machine — the removable parts are the `mmap` (0.3–0.4 µs) and
the `mprotect` (~0.4 µs), i.e. at most ~0.02 % of the run.

Non-time cost, which is real and is *not* what this lane was asked to optimise:
12 808 blocks × one 16 KiB page = **200 MB of resident code memory** (the map
below confirms every block owns a page; the process reported a 318 MB physical
footprint). That is ~19× the bytes if blocks shared pages. See §5 for why that
cannot be fixed from this crate.

## 4. W^X: already batched, and batching further would be unsound

The question "does permission toggling flush the whole i-cache per block" has a
concrete answer from §2: **no.** `Memory::set_readable_and_executable` iterates
`self.allocations[self.already_protected..]` and then sets
`already_protected = allocations.len()`, so each mapping is transitioned and
flushed once; the flushed range is the block's own new 16 KiB page, never the
arena. `pipeline_flush_mt` is a no-op on this OS. The measured cost of that
per-block flush is 1.1–1.2 µs (§3).

"Batch it" is nevertheless tempting, so record why it is refused:

- **Correctness forbids deferring the transition.** A block is chained into
  (`bl` from its predecessor, `WIE_JIT_CHAIN`) as soon as it is installed, and
  it may be entered on another thread before any later block compiles. Code that
  is still `PROT_WRITE` cannot be entered, so the transition cannot be batched
  behind a counter. The only sound batch point is "just before first execution",
  which this JIT does not have — `finalize_definitions()` per block *is* that
  point.
- **The `MAP_JIT` alternative is worse here, not better.** `MAP_JIT` +
  `pthread_jit_write_protect_np` exists to let a thread flip between a writable
  and an executable alias without touching page tables. It is a *per-thread*
  state: while the state is "write", that thread cannot execute JIT code. This
  engine has **4 background compile workers** writing while guest threads
  execute the code the workers produce, plus the guest thread itself compiling
  inline. Making that correct means every write path in `lower` and the worker
  loop carry a thread-state flip, and a mistake presents as a SIGBUS or a stale
  fetch in guest code — for a mechanism whose entire upside is the 0.3–0.4 µs
  `mmap` it would replace (~4 ms per run).
- **The one honest remaining cost is the 16 KiB page granularity,** and that is
  an allocator-granularity problem, not a permission problem (§5).

## 5. The blocker: `JITMemoryKind` is not re-exported, so we cannot write a provider

Everything a reader would want to do instead — pack several blocks per page,
count code bytes per compile, de-alias block starts, batch the transition — runs
through one public trait method:

```rust
// cranelift-jit 0.133.1, memory/mod.rs
pub trait JITMemoryProvider {
    fn allocate(&mut self, size: usize, align: u64, kind: JITMemoryKind) -> io::Result<*mut u8>;
    unsafe fn free_memory(&mut self);
    fn finalize(&mut self, branch_protection: BranchProtection) -> ModuleResult<()>;
}
```

`JITMemoryKind` is **not** in the crate's re-export list — `lib.rs` has
`mod memory;` then `pub use crate::memory::{ArenaMemoryProvider,
BranchProtection, JITMemoryProvider, SystemMemoryProvider};`. So the trait is
nameable but not *implementable* from outside, because the implementor cannot
write the signature. Verified by compiling exactly that:

```rust
impl JITMemoryProvider for P {
    fn allocate(&mut self, size: usize, align: u64, kind: cranelift_jit::JITMemoryKind)
        -> std::io::Result<*mut u8> { unimplemented!() }
    // …
}
```
```
error[E0425]: cannot find type `JITMemoryKind` in crate `cranelift_jit`
```

What remains reachable is the stock `ArenaMemoryProvider`, which was implemented
and measured (§9). It does **not** pack: `allocate_inner` only reuses a segment
that is `!finalized`, and `finalize()` (called per block) marks every segment
finalized, so the next allocation takes the `allocate_segment(size)` path and
rounds `size` up to 16 KiB — the same one-page-per-block layout as the default
provider, minus the `mmap` syscall.

**The fix is upstream (re-export `JITMemoryKind`).** With that one-line change,
a provider in `crates/wie-cpu` could suballocate many blocks into one page and,
more importantly, place each block at a *different page offset* — which is the
only local fix for §7. Until then this crate's options are: the stock providers
(measured below), a vendored/patch fork of cranelift-jit (a maintenance cost this
lane declined to take on unilaterally), or nothing.

## 6. Where the time actually is, and how big the code working set is

`sample <pid> 8 -mayDie` over a 100 s DOOM Retro run, 1 ms interval, with the
block map from `WIE_JIT_MAP` used to classify PCs (`sample` reports JIT'd code as
unknown and cranelift's symbol registry is invisible to it). Leaf samples,
main thread:

| where the main thread's leaf PC is | share |
|---|---|
| host code in the `wie` binary (dispatcher, memory/string helpers, WinAPI) | **64.0 %** |
| JIT'd guest code | **26.4 %** |
| idle (`__psynch_cvwait`, parks) | 6.4 % |
| system calls | 3.3 % |

(The 4 compile workers were 77–88 % idle in that window, the guest worker thread
27.8 % in JIT code. The 8 s window is not the whole run.)

So **JIT-resident time is 26.4 % of the main thread** — the hard ceiling on any
code-memory or locality fix, before asking how much of *that* is stall.

Within the JIT slice, from the same sample plus the map:

- **514 distinct block pages** were ever sampled, **313 blocks at leaf** (290 on
  the main thread). The run compiled **12 808** blocks.
- Those PCs fall in **1 219 distinct 128-byte i-cache lines = 152 KiB** of
  touched code — a lower bound on the working set, since a sampler only sees
  lines that were executing. L1I is 128 KiB, so the whole sampled footprint is
  ~1.2× one L1I, and the *hot* part is far smaller.
- Cumulative over main-thread JIT samples: top 1 block = 9.1 %, top 2 = 13.0 %,
  **top 8 = 27.7 %**, top 16 = 37.1 %, top 32 = 50.1 %, top 128 = 84.2 %,
  top 256 = 97.7 %. Half the JIT-resident time is 32 blocks.
- The run reports `block_entries=149 492 035` and `insn_per_entry=226.9`: **the
  average block entry retires 227 guest instructions**, i.e. the hot loops are
  self-looping superblocks whose inner loop is *inside* one compiled function.
  That single number is what bounds §7.

**Locality of the hot set** (host pages from the map, `#n` = definition order):

| rank | host page | guest VA | insns | % of JIT time | defined |
|---|---|---|---|---|---|
| 1 | `0x161bb0000` | `0x1400e1704` | 9 | 9.1 % | #9183/12808 |
| 2 | `0x1619f4000` | `0x1400e0d50` | 11 | 3.9 % | #9072 |
| 3 | `0x161bb4000` | `0x1400e16f5` | 3 | 3.3 % | #9184 |
| 4 | `0x161bb8000` | `0x1400e16fd` | 3 | 3.3 % | #9185 |
| 5 | `0x11f504000` | `0x14015538d` | 10 | 3.3 % | #6709 |
| 6 | `0x11e904000` | `0x14013a420` | 3 | 1.7 % | #5941 |
| 7 | `0x15fa40000` | `0x1401553c1` | 2 | 1.5 % | #7043 |
| 8 | `0x15ff50000` | `0x1401553d1` | 3 | 1.5 % | #7367 |

Blocks 3 and 4 are guest-adjacent (8 bytes apart) and *are* host-adjacent
(`+0x4000`, `+0x8000`), i.e. placement does follow creation order — but the two
hottest blocks are 1.8 MB apart, the eight together span **1075 MB of address
space**, and **3 237 other blocks' pages** lie between the lowest and highest of
them. If "group the hot loop's blocks together" were the lever, that is the
scattering it would fix.

**It is not the lever**, and the reason is §3 + §6's `insn_per_entry`: a hot
block's inner loop runs 227 guest instructions *inside* the block before any
other block is entered, so cross-block i-cache rotation happens 227× less often
than the instruction count suggests.

## 7. The one genuine defect found: L1I aliasing of page-aligned block starts

Every block starts at **offset 0 of a 16 KiB page** (§2 step 3), and 16 KiB is
exactly the L1I index period for a 128 KiB, 128-byte-line, 8-way instruction
cache: 128 sets × 128 B = 16 KiB. So **the first line of every block maps to set
0, the second to set 1, and so on** — all blocks alias each other's set sequence
by construction.

Isolated measurement of the mechanism (`clang -O2`, `MAP_JIT` region, 16 stubs of
33 instructions = 2 lines each, called round-robin through `blr`; layout A puts
each stub at page offset 0, layout B at a distinct offset per stub):

```
pthread_jit_write_protect_supported_np()=1
round 0: page-aligned 10.07 ns/call   de-aliased 2.44 ns/call   ratio 4.120
round 1: page-aligned 10.57 ns/call   de-aliased 2.37 ns/call   ratio 4.454
round 2: page-aligned  8.90 ns/call   de-aliased 2.17 ns/call   ratio 4.090
```

**4× slower page-aligned**, consistently, with identical code and identical page
count in both layouts (so this is not iTLB; it is i-cache conflict misses, ~2
lines refilled per 33-instruction call).

**Why that 4× does not transfer to wie, with the numbers:** the benchmark rotates
16 stubs every 33 instructions — no reuse inside a stub. A wie block executes
**227 guest instructions per entry** inside one function, so a full 2-line refill
(~30 cycles) is amortised over the whole entry. Derivation: 33.9 G guest
instructions of JIT-resident work over ~26 s (§6) is ≈2.7 host cycles per guest
instruction at 3.5 GHz, so an average entry is ≈690 cycles of work and one line
refill is ≈2 % of it. Even assuming *every* entry refilled its two hottest lines,
the bound is **≲3 % of JIT time = ≲0.8 % of a 100 s run** — and the 152 KiB
working set with the top 8 blocks holding 27.6 % says the hot blocks are resident
while they loop, so the real figure is below that bound.

Recorded anyway because it is the only code-memory property of this engine that
is *wrong* rather than merely cheap, it is invisible to every counter we have,
and it will be re-derived by the next reader who runs `sample` and sees scattered
block addresses. **The fix is upstream (re-export `JITMemoryKind`)**: a provider
that packs blocks into shared pages at de-aliased offsets, or simply rounds
`position` to the next 256 B instead of the next 16 KiB, removes the aliasing and
the 200 MB footprint in one change (§5).

## 8. Prefetch / warm-up at restore: cannot pay

The persistent code cache (`WIE_JIT_CODE_CACHE`) restores by *defining* a
function from persisted bytes, so a restored block already goes through
`define_function_bytes` → `finalize_definitions` → one `mprotect` + one 16 KiB
`ic ivau` flush (~1.2 µs, §3). A warm-up on top of that was implemented both
ways and is a no-op in the only direction that matters:

- **Cost avoided:** ~2–4 ms of Cranelift per block. **Cost paid:** 1.2 µs of
  flush that is unavoidable anyway. The prefetch adds work to a path that is
  already 1 000× cheaper than what it replaced.
- **The warm-up target is wrong.** Fetch does not read through the data cache, so
  touching the bytes warms nothing useful; AArch64 `PRFM PLI*` is the only
  user-space hint that reaches the instruction cache. And it would be hinting at
  cold code: the arena holds ~200 MB of code pages of which the sampled *ever
  executed* footprint is ~8 MB (514 pages), and the hot set is 152 KiB.
- **There is no bulk restore to amortise over.** Restores are per block,
  interleaved with execution, not a batch at load time — so "prefetch the code
  range once at restore" has no range to name.

## 9. What was implemented, measured, and deleted

Two default-off knobs were built (a `crates/wie-cpu/src/jit/code_arena.rs`
installing `ArenaMemoryProvider` under `WIE_JIT_CODE_ARENA`, and a `prfm
plil2keep` warm-up under `WIE_JIT_CODE_PREFETCH` wired into `JitShared`'s restore
path), documented in the RUNBOOK, and then **reverted**: with §3's numbers a
switch that buys ≤0.04 % is dead weight in every future bisect and has to be
documented forever. Nothing of that remains in the tree.

The arena's end-to-end measurement is recorded only because "we tried it" is
worth something. 100 s DOOM Retro, `--screenshot`, interleaved, compiled block
count and per-run numbers from `WIE_RUNTIME_PROFILE`:

| arm | frames_published | compile_us | bg_compiles | wall_ms |
|---|---|---|---|---|
| A (system provider, default) | 1397 | 14 598 429 | 9521 | 99 625 |
| B (arena) rep 1 | 1198 | 24 354 879 | 8537 | 89 819 |
| B (arena) rep 2 | 1415 | 16 638 302 | 8859 | 99 970 |

Uninterpretable by construction, and that is the honest report: the *within-arm*
spread already exceeds any between-arm difference (B alone varies 1198→1415
frames and 16.6→24.4 s of compile), one B rep exited early (the `--screenshot`
entry returns when the guest first goes idle, so its wall time is not the full
cap), and the predicted effect — removing ~4 ms of `mmap` per run — is four
orders of magnitude below this resolution. A/B on a loaded host cannot arbitrate
a 0.04 % question; the isolated microbenchmark in §3 is the measurement, and the
end-to-end run exists only to confirm that nothing pathological happened. (It
did not.)

## 10. What would actually move this run's needle

Recorded so the next reader does not re-open code memory looking for it. From the
same 100 s run (`WIE_RUNTIME_PROFILE=1`, `--screenshot`, 588 frames published in
the cleanest rep):

- `compile_us = 21 385 095` (21.4 s) over `bg_compiles = 8524` + 47 inline, on 4
  background workers that ran **77–88 % busy** in the sampled window; a second
  rep with a sampler attached reported `compile_us = 40 492 286` over 9404
  compiles. That is ~2–4 ms **per block**, thousands of blocks per run, i.e.
  **21–40 s of a 100 s run spent compiling** — five hundred to a thousand times
  the entire code-memory path.
- 33.9 G retired guest instructions at `jit_share_pct = 99.8`, so the JIT is
  doing the work; the cost is producing it.

So: compile *selection* and compile *cost*, not code memory. See
[`compile-selection-policy.md`](compile-selection-policy.md) and
[`performance-ceiling.md`](performance-ceiling.md); the code-cache
(`WIE_JIT_CODE_CACHE`) is the only lever in this neighbourhood that targets the
real number, by not compiling the same block twice across runs.

## 11. Reproducing every number here

```bash
# §1 — what the OS allows (write to /tmp, no repo changes)
clang -O2 -o /tmp/probe probe.c && /tmp/probe     # probe source is §1's snippet set
sysctl hw.pagesize hw.l1icachesize hw.cachelinesize
codesign -dv target/release/wie 2>&1 | grep -E 'flags|Signature'

# §3 — per-block code-memory cost, sequenced like cranelift-jit does it
clang -O2 -o /tmp/bench bench.c && /tmp/bench     # 12808 × (mmap + touch + mprotect + ic ivau)

# §6 — attribution and working set
WIE_RUNTIME_PROFILE=1 WIE_JIT_MAP=/tmp/doom.map ./target/release/wie run \
  --screenshot /tmp/x.bmp --bottle doomretro --app-dir real_exes/doomretro \
  real_exes/doomretro/doomretro.exe -iwad freedoom2.wad &
sample <pid> 8 -mayDie -file /tmp/doom.sample.txt
# classify each sampled PC against /tmp/doom.map; block pages are 16 KiB-aligned,
# one per block, so `addr & ~0x3fff` identifies the block.

# §7 — the aliasing mechanism in isolation
clang -O2 -o /tmp/alias alias.c && /tmp/alias     # 16 stubs, page-aligned vs de-aliased
```

Caveats that apply to every figure above: single run per arm; the host was
carrying other work (load 3.7–5.7/8); the `sample` window is 8 s of a 100 s run
and stationarity across the run is assumed, not measured; `sample`'s own 1 ms
interrupt load perturbs the run it measures (the sampled rep published 286
frames against 588 for the cleanest unsampled rep). The per-block microbenchmarks
in §3 and the aliasing ratios in §7 are the load-independent results; the
end-to-end frame and wall numbers are not results at all.
