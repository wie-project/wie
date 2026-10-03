//! Two emulated sessions in one process must not share a guest heap.
//!
//! The UCRT `malloc`/`free` fast path is reached only from compiled code, so
//! the honest way to test it is to plant a `Ready` block whose body is a
//! hand-written trampoline calling the very same helpers Cranelift imports.
//! That drives the WHOLE publish chain under test:
//!
//! ```text
//! configure_fast_path → JitShared::heap → run_compiled's JitCtx → heap_layout(ctx)
//! ```
//!
//! — not just the reader in isolation. Before this fix the layout lived in
//! process-global `static`s and the large free list in a process-wide
//! `Mutex<Vec<..>>`, so the second engine's init silently repointed the first
//! engine's allocator and wiped its large free list.

use super::*;
use crate::jit::fast_api::{JitFastPathConfig, JitHeapLayout, wie_ucrt_free, wie_ucrt_malloc};
use crate::jit::lower::CompiledBlock;
use crate::jit::tier::OptTier;
use crate::mem::GuestMemory;

/// Stand-in for a compiled block that ends in a fast-path `malloc`: take the
/// size from RCX (the register the lowering passes it in), leave the returned
/// pointer in RAX. `gpr_dirty_bits` stays 0, which `run_compiled` reads as
/// "full writeback", so the pointer lands back in the engine's `RegFile`.
#[expect(unsafe_code)]
unsafe extern "C" fn probe_malloc(ctx: *mut JitCtx) {
    // SAFETY: entered only from `JitCpu::run_compiled`, which owns the
    // `JitCtx` for the whole native frame and does not touch it until the
    // frame returns. Same claim the `wie_ucrt_*` helpers themselves make.
    unsafe {
        let size = (*ctx).gpr[1];
        (*ctx).gpr[0] = wie_ucrt_malloc(ctx, size);
    }
}

/// As [`probe_malloc`], for `free`: the pointer comes from RCX.
#[expect(unsafe_code)]
unsafe extern "C" fn probe_free(ctx: *mut JitCtx) {
    // SAFETY: as in `probe_malloc` — one owned frame, no intervening access.
    unsafe {
        let ptr = (*ctx).gpr[1];
        wie_ucrt_free(ctx, ptr);
        (*ctx).gpr[0] = ptr; // echo, so the caller can assert it ran
    }
}

/// A guest heap region plus its control block, at addresses of the test's
/// choosing so two engines can be given provably disjoint layouts.
struct TestHeap {
    ctrl: u64,
    base: u64,
    end: u64,
}

impl TestHeap {
    /// Map `ctrl` (one page) and the `[base, end)` payload region into `mem`,
    /// with the bump cursor seeded at `base`.
    fn map(&self, mem: &mut GuestMemory) {
        mem.virtual_alloc(
            self.ctrl,
            0x1000,
            crate::mem::MEM_RESERVE | crate::mem::MEM_COMMIT,
            protect::PAGE_READWRITE,
        )
        .expect("map ctrl page");
        mem.virtual_alloc(
            self.base,
            usize::try_from(self.end - self.base).expect("heap size fits usize"),
            crate::mem::MEM_RESERVE | crate::mem::MEM_COMMIT,
            protect::PAGE_READWRITE,
        )
        .expect("map heap region");
        mem.write(self.ctrl, &self.base.to_le_bytes())
            .expect("seed bump cursor");
    }

    fn layout(&self) -> JitHeapLayout {
        JitHeapLayout {
            ctrl_va: self.ctrl,
            base: self.base,
            end: self.end,
        }
    }

    fn contains(&self, va: u64) -> bool {
        va >= self.base && va < self.end
    }
}

/// Map a `Ready` block at `rip` whose body is `func`, so dispatching there runs
/// `func(ctx)` natively through `run_compiled` — the same entry every real
/// compiled block takes.
fn plant(cpu: &mut JitCpu, rip: u64, func: unsafe extern "C" fn(*mut JitCtx)) {
    cpu.insert_ready(
        rip,
        CompiledBlock {
            func,
            func_id: None,
            tier: OptTier::Base,
            insn_count: 1,
            guest_start: rip,
            guest_end: rip + 8,
            inv_gen: 0,
        },
    );
    assert!(cpu.has_ready_at(rip), "probe block must be Ready");
}

/// Run the planted probe once with `arg` in RCX; return RAX.
fn run_probe(cpu: &mut JitCpu, rip: u64, arg: u64) -> u64 {
    cpu.thread.regs.rip = rip;
    cpu.thread.regs.set_gpr_public(1, arg);
    cpu.run_until_stop(rip, 0, 0, 1, 0, 0)
        .expect("probe dispatch");
    cpu.thread.regs.gpr(0)
}

const PROBE_A: u64 = 0x1400_0000;
const PROBE_B: u64 = 0x1500_0000;

/// Session A's heap: control page at `0x3000_0000`, 1 MiB payload after it.
const HEAP_A: TestHeap = TestHeap {
    ctrl: 0x3000_0000,
    base: 0x3001_0000,
    end: 0x3011_0000,
};

/// Session B's heap: a different control page and a different payload region,
/// so "A allocated from B's heap" is observable rather than inferred.
const HEAP_B: TestHeap = TestHeap {
    ctrl: 0x4000_0000,
    base: 0x4001_0000,
    end: 0x4011_0000,
};

/// Build one engine with `heap` mapped but NOT yet configured, and no probe
/// planted. Splitting it this way is deliberate: `configure_fast_path` clears
/// the compiled cache, so a test must plant its probe *after* configuring, and
/// the whole bug is about *when* each session configures relative to the other.
fn engine_with_heap(heap: &TestHeap) -> JitCpu {
    let cpu = JitCpu::open_x86_64();
    let mut mem = cpu.shared_jit().mem.write().expect("mem lock");
    heap.map(&mut mem);
    drop(mem);
    cpu
}

/// Session init: install `heap` as this engine's UCRT fast-path heap, then
/// plant the `malloc` probe at `probe` (after, because `configure_fast_path`
/// clears the compiled cache).
///
/// No `(VA, kind)` pairs — the probes call the helpers directly, so no fake-API
/// VA table is needed to reach them.
fn init_session(cpu: &mut JitCpu, heap: &TestHeap, probe: u64) {
    cpu.configure_fast_path(JitFastPathConfig {
        heap: heap.layout(),
        pairs: Vec::new(),
    });
    plant(cpu, probe, probe_malloc);
}

/// A second engine's initialisation must not repoint the first engine's
/// allocator.
///
/// Before the fix, `configure_fast_path` wrote process-global statics, so after
/// B was configured, A's `malloc` read B's `ctrl_va` — unmapped in A's own
/// `GuestMemory` — and returned 0. Now A keeps its own layout and its second
/// allocation lands in A's region.
#[test]
fn second_session_init_does_not_repoint_first_session_allocator() {
    let mut a = engine_with_heap(&HEAP_A);
    let mut b = engine_with_heap(&HEAP_B);

    // --- Session A alone: configured, allocating, all healthy. ---
    init_session(&mut a, &HEAP_A, PROBE_A);
    let a1 = run_probe(&mut a, PROBE_A, 0x40);
    assert!(HEAP_A.contains(a1), "A's first malloc must be in A's heap");
    assert_ne!(a1, 0, "A's first malloc must succeed");

    // --- Session B initialises in the same process, with its own layout. ---
    init_session(&mut b, &HEAP_B, PROBE_B);
    let b1 = run_probe(&mut b, PROBE_B, 0x40);
    assert!(HEAP_B.contains(b1), "B's first malloc must be in B's heap");
    assert_ne!(a1, b1, "the two sessions must not alias one block");
    let b2 = run_probe(&mut b, PROBE_B, 0x40);
    assert!(HEAP_B.contains(b2), "B's second malloc stays in B's heap");
    assert_ne!(b1, b2, "B's own allocations must not overlap");

    // --- The decisive step: A allocates again, after B configured and after
    // B had already handed out blocks. Pre-fix this is where A breaks. ---
    let a2 = run_probe(&mut a, PROBE_A, 0x40);
    assert_ne!(a2, 0, "A's malloc must not fail after B's init");
    assert!(
        HEAP_A.contains(a2),
        "A must keep allocating from its own heap, got {a2:#x}"
    );
    assert!(
        !HEAP_B.contains(a2),
        "A must never allocate out of B's heap, got {a2:#x}"
    );
    assert_ne!(a1, a2, "A's two live blocks must not overlap");

    // A's `free` must also still accept A's own blocks (it range-checks the
    // pointer against the layout, so a repointed layout rejects them).
    let echoed = run_probe_free(&mut a, PROBE_A, a1);
    assert_eq!(echoed, a1, "A must be able to free its own block");
}

/// A large free list must survive another engine's initialisation.
///
/// This is the worse half of the bug: `LARGE_FREE` was a process-wide
/// `Mutex<Vec<(u64,u64)>>` that `install_heap_layout` cleared on ANY session's
/// `configure_fast_path`. So B's init threw away A's freed large blocks, and
/// A's next large `malloc` re-bump-allocated memory that was still live.
///
/// The assertion is exact: a `malloc(S)` / `free` / `malloc(S)` round trip must
/// return the SAME address, because the second `malloc` is served by best-fit
/// from the engine's own list. Lose the list and the address differs.
#[test]
fn large_free_list_survives_second_session_init() {
    // Above `LARGE_THRESHOLD` (64 KiB), so the block goes on the large list
    // rather than a size-class head. 16-byte aligned so `round_up_size` is the
    // identity and the header records the exact request.
    const BIG: u64 = 0x2_0000;

    let mut a = engine_with_heap(&HEAP_A);
    let mut b = engine_with_heap(&HEAP_B);

    // A is configured and alone: its large block and its free list are its own.
    init_session(&mut a, &HEAP_A, PROBE_A);
    let big1 = run_probe(&mut a, PROBE_A, BIG);
    assert!(HEAP_A.contains(big1), "A's large malloc is in A's heap");
    let echoed = run_probe_free(&mut a, PROBE_A, big1);
    assert_eq!(echoed, big1, "free must accept A's own large block");

    // B initialises and allocates — the call that used to `LARGE_FREE.clear()`,
    // in the state where A holds a freed large block.
    init_session(&mut b, &HEAP_B, PROBE_B);
    let b1 = run_probe(&mut b, PROBE_B, BIG);
    assert!(HEAP_B.contains(b1), "B's large malloc is in B's heap");
    assert_ne!(b1, big1, "the two sessions must not alias one block");

    // A's identical large request must come back off A's own free list.
    let big2 = run_probe(&mut a, PROBE_A, BIG);
    assert_eq!(
        big2, big1,
        "A's freed large block must be reused; a different address means \
         B's init wiped A's large free list"
    );

    // And a DIFFERENT large size must not be served A's freed block, or the
    // list would be handing out a block too small for the request.
    let bigger = run_probe(&mut a, PROBE_A, BIG * 2);
    assert!(
        HEAP_A.contains(bigger) && bigger != big1,
        "an oversized request must not reuse the smaller freed block"
    );
}

/// Dispatch the `free` probe. Separate from [`run_probe`] only so the planted
/// body is explicit at each call site.
fn run_probe_free(cpu: &mut JitCpu, rip: u64, ptr: u64) -> u64 {
    // Re-plant at a scratch VA carrying the `free` body: the block is a
    // compile unit, and this keeps `engine_with_heap`'s single plant honest.
    let free_va = rip + 0x100;
    plant(cpu, free_va, probe_free);
    cpu.thread.regs.rip = free_va;
    cpu.thread.regs.set_gpr_public(1, ptr);
    cpu.run_until_stop(free_va, 0, 0, 1, 0, 0)
        .expect("free dispatch");
    cpu.thread.regs.gpr(0)
}

/// A fresh engine that never called `configure_fast_path` must take the slow
/// path, not allocate against whatever a previous engine installed.
///
/// This is the default the fix makes explicit: `heap_ctrl_va == 0` disables
/// the fast path. Before the fix, an un-configured engine in the same process
/// silently inherited the last engine's layout.
#[test]
fn unconfigured_engine_allocates_nothing() {
    let mut cpu = JitCpu::open_x86_64();
    plant(&mut cpu, PROBE_A, probe_malloc);
    let p = run_probe(&mut cpu, PROBE_A, 0x40);
    assert_eq!(p, 0, "an unconfigured engine has no guest heap to serve");

    // Same process, after another engine installed a real layout: still 0.
    let mut other = engine_with_heap(&HEAP_A);
    init_session(&mut other, &HEAP_A, PROBE_B);
    let served = run_probe(&mut other, PROBE_B, 0x40);
    assert!(HEAP_A.contains(served), "the configured engine still works");
    let p2 = run_probe(&mut cpu, PROBE_A, 0x40);
    assert_eq!(p2, 0, "another engine's layout must not leak into this one");
}
