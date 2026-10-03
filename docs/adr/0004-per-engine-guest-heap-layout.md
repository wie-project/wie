# ADR 0004 — Per-engine guest heap layout for the UCRT fast path

Status: Proposed · 2026-10-01 — maintainability plan T3.2

## Context

The JIT's UCRT `malloc`/`free` fast path stores the guest heap layout in
three process-global `static AtomicU64`:

```
crates/wie-cpu/src/jit/fast_api.rs:90-92   HEAP_CTRL / HEAP_BASE / HEAP_END
```

written by `install_heap_layout` (`fast_api.rs:106-108`), whose sole caller is
`JitCpu::configure_fast_path` (`jit/pipeline.rs:159`).

Measured threading facts (established by reading every site):

- **Read sites: exactly two.** `heap_layout()` (`fast_api.rs:115-121`), called
  only from `wie_ucrt_malloc` (`:218`) and `wie_ucrt_free` (`:273`). No other
  reader anywhere in the workspace.
- **Write site: exactly one.** `install_heap_layout`.
- **The consumers are per-guest-thread.** `wie_ucrt_malloc`/`free` are
  Cranelift import symbols invoked from compiled code on whichever guest thread
  happens to be running. Per CLAUDE.md each guest thread owns its own
  `JitCpu`, and worker engines never call `configure_fast_path` — they read the
  statics.

So this is load-bearing cross-engine state shaped like a constant. Two
consequences follow, and the second is worse:

1. Two `RuntimeSession`s in one process — or any reconfigure after execution
   starts — makes every engine's `malloc`/`free` use the last-installed layout.
   Session A allocates from session B's heap.
2. `LARGE_FREE` (`fast_api.rs:102`) is a *separate* process-wide
   `Mutex<Vec<(u64,u64)>>` whose contents are cleared by **any** session's
   `configure_fast_path` (`:110-112`). Session B's init therefore wipes session
   A's large free list, and A's outstanding large blocks get re-bump-allocated
   while still live.

`guest_layout.rs:1-13` justifies sharing *constants* across crates. It does not
justify sharing *mutable heap state*.

## Decision

Move the layout onto `JitCtx` as three plain `u64`, published per `run_compiled`
from the `Arc<JitShared>` the session configured.

Plain `u64` rather than `AtomicU64`: `run_compiled` owns the context
exclusively for the whole native frame, and `configure_fast_path` runs at init.
The genuine cross-thread race lives in `JitShared`, which keeps the atomics
where they are actually needed.

1. `jit/lower/mod.rs` — four fields on `JitCtx`, after `insn_acc` (`:383`):
   `heap_ctrl_va`, `heap_base`, `heap_end` (plain `u64`, host-helper-only — the
   emitted IR never reads them, so no offset constants) and
   `large_free: *mut LargeFreeList`. `heap_ctrl_va == 0` disables the UCRT
   fast path entirely.
2. `jit/shared.rs` — `heap: [AtomicU64; 3]` on `JitShared` and in
   `JitShared::new()`, so `configure_fast_path` stays race-free against a
   running frame; `large_free: LargeFreeList` on `PerThreadJitState`.
3. `jit/pipeline.rs` — `configure_fast_path` `Release`-stores the three layout
   values into `shared.heap` and clears **its own** `self.thread.large_free`;
   the `JitCtx { … }` literal in `run_compiled` `Acquire`-loads them and takes
   `&raw mut self.thread.large_free`.
4. `jit/fast_api.rs` — delete the three statics, `static LARGE_FREE`,
   `install_heap_layout` and the no-arg `heap_layout()`; add `heap_layout(ctx)`
   and `large_free_mut(ptr)`. Both call sites read the layout **before**
   `mem_mut(ctx)`, which takes `&mut`.

`JitHeapLayout` stays `pub`, so `wie-runtime`'s `configure_fast_path` call site
needs no change.

**`LARGE_FREE` moved with the layout, not after it.** It was originally listed
as a deliberate follow-up on the grounds that a `Vec` inside a `#[repr(C)]`
C-ABI struct deserved its own decision. Implementation showed that reasoning was
wrong in a way worth recording: `#[repr(C)]` would have been *fine* with a `Vec`
field. The real blocker is lifetime — `JitCtx` is constructed fresh inside
`run_compiled` (one per block dispatch) and dropped when the frame returns, so an
owned `Vec` would be destroyed at **every block boundary**: the large free list
would always be empty on entry, every large `malloc` would bump-allocate, and
the list's entire reason for existing would be gone. That is a silent severe
performance regression rather than a compile error, which makes it worse than a
build break.

So the `Vec` is owned by `PerThreadJitState` (per engine, persists across
frames) and `JitCtx` carries `large_free: *mut LargeFreeList` — byte-for-byte
the existing `chain_slots: *mut ChainSlot` idiom, so no new pattern is
introduced. `large_free_mut` is a *safe* fn holding one `unsafe` block, matching
the shape of `mem_mut`, so the call sites carry no new `# Safety` burden. Net
effect: the `Mutex` is gone from the large-alloc/free path entirely.

**Ordering is `Release`/`Acquire`, not `Relaxed`.** The copy onto `JitCtx` must
not observe a layout published after the frame started. This pairing does not
make the triple atomic, so a genuine mid-frame reconfigure could still be
observed torn — that is a caller bug, not something this ordering can fix — but
it costs one acquire per *block dispatch*, not per `malloc`.

## Alternatives considered

- **Keep the statics, document the single-session assumption.** Cheapest, but
  leaves a silent cross-session heap aliasing and a large-free-list wipe. A
  documented assumption is not a check; the JIT is a library that
  `wie-runtime` may hold two of.
- **Thread-local layout instead of a ctx field.** Works, but adds TLS access to
  the hot path for no benefit over a field already in hand.
- **Move `fast_api.rs` out of `wie-cpu` into `wie-runtime`/a new `wie-ucrt`.**
  Best layering, but it needs a `FastApiKind` edge that `IcedCpu` also wants, so
  it requires an enum or trait back in `wie-cpu`. Deferred — it is a packaging
  question, not a correctness one, and this decision already removes the bug.

## Consequences

- `crates/wie-winapi`'s 24-size-class allocator, bump allocation, large-list
  fitting and block headers are unchanged in behaviour.
- The four edits are **interdependent; no subset compiles.** They must land as
  one change. This is currently blocked on `jit/lower/mod.rs` being unfenced.
- A per-session engine that never called `configure_fast_path` reads
  `heap_ctrl_va == 0` and takes the slow path — the correct default, and a
  behaviour that was previously implicit in "the statics hold whatever was
  installed".

### Behaviour change worth knowing

The large free list is now **per engine** rather than per session, so a large
block freed on guest thread A is no longer reusable by guest thread B — B
bump-allocates instead. That is safe (bump allocation is always valid) and is
the trade that buys the mutex removal. It applies only to blocks above
`LARGE_THRESHOLD` (64 KiB), and only when `WIE_GUEST_HEAP` is off — with the
accelerator on, the IAT points at in-guest code and this fast path is bypassed
entirely. The `GuestHeap` on the WinAPI path is untouched and still session-wide
under its own mutex. `configure_fast_path` still clears the list, now correctly
scoped to the engine being reconfigured, which preserves the original "a new
session drops stale heap-relative VAs" intent without reaching other engines.

## Validation

**Implemented and passing** — `crates/wie-cpu/src/jit/tests/heap_isolation_tests.rs`,
3 tests. It does not poke internals: each test plants a `Ready` block whose body
is a hand-written `extern "C"` trampoline calling the very same
`wie_ucrt_malloc`/`wie_ucrt_free` Cranelift imports, then dispatches it. That
drives the whole chain under test (`configure_fast_path → JitShared::heap →
run_compiled`'s `JitCtx` literal → `heap_layout(ctx)`) rather than just the
reader. Two engines, each with its own `JitShared` *and* `GuestMemory`, given
provably disjoint layouts.

- `second_session_init_does_not_repoint_first_session_allocator` — A allocates,
  B initialises and allocates, then **A allocates again**. Asserts A's result is
  inside A's range, outside B's, and distinct from A's first block. **Pre-fix
  this fails with `left: 0`** — A read B's `ctrl_va`, unmapped in A's memory.
- `large_free_list_survives_second_session_init` — A mallocs `0x2_0000` (> the
  64 KiB threshold) and frees it; B initialises; A requests the same size and
  must get **the same address** back, while a doubled request must *not* be
  served the smaller freed block. **Pre-fix this fails** — B's init had cleared
  the process-wide list.
- `unconfigured_engine_allocates_nothing` — an engine that never called
  `configure_fast_path` returns 0. Honest caveat: this one **also passes
  pre-fix**, because nextest gives each test its own process and the old statics
  are still zero. It guards the new explicit default; it is not fail-before
  evidence.

Regressions checked: `cargo nextest run -p wie-cpu` → 281 passed (baseline 278,
+3 new, none lost); `cargo check --workspace` clean; clippy shows 4 warnings
before and after, all pre-existing in the abandoned agent's test file.

**Integration coverage (run 2026-10-03, after implementation).** Both real
guests now exercise the allocator, which is the part this change altered:

- **`2048.exe`** (fetched via `scripts/fetch.sh 2048`) runs identically with
  the default fast path and with `WIE_GUEST_HEAP=1`: same guest output, same
  terminal `caller_rip` (`0x14000140d`), `ExitProcess code=0` both ways. The
  accelerator rewires the IATs to in-guest code and so bypasses this path
  entirely, which makes the pair a useful cross-check rather than two runs of
  the same thing.
- **`7za.exe`** is the heavier allocator and is the more relevant probe for the
  large-list change: it ran ~90 handled APIs — a long interleaved
  `msvcrt.dll!malloc`/`free`/`memmove`/`fputs` sequence — and terminated
  cleanly through `msvcrt.dll!exit`. Nothing regressed, and the guest reached a
  consistent terminal state.

`7za.exe` cannot be driven with arguments outside micro mode
(`--persistent` rejects guest argv) and hits `ApiLimit` in micro mode, so the
run exercises initialisation and teardown rather than an archive operation.
That is still the path that allocates most heavily during start-up.

**Still not covered:** a multi-threaded real guest exercising the *large*
(`>64 KiB`) free list across engines. The per-engine scoping means a block freed
on one guest thread is not reused by another — safe, but only proven by the
in-crate `heap_isolation_tests.rs`, not by a real guest.

## Reversibility

Structural rather than flag-gated: reverting is a four-file revert. Behaviour is
unchanged in the single-session case, so it can ship without an env switch — a
kill-switch here would only protect a configuration the change fixes outright.

The large-list change is the one with a visible behavioural delta (per engine,
not per session — see Consequences). If that regresses block reuse under
`WIE_GUEST_HEAP`, the targeted revert is the `large_free` pointer field and its
`PerThreadJitState` owner; the three layout `u64`s are independent of it.