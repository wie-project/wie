// Shared dual-backend test harness for the `jit::tests` tree.
//
// Every test here runs the same guest bytes on both engines — `IcedCpu` as the
// reference, `JitCpu` as the thing under test — so that plumbing lives in one
// place instead of four. It used to be copy-pasted: `atomic_tests` (the most
// complete, and the only one with `open_worker`), `cmpxchg_tests`,
// `chain_tests`, and the `simd_dual` / `gs_teb_dual` pair in `mod.rs`, with the
// other files reaching `atomic_tests`' copy across module paths.
//
// ## The one rule: a distinct code VA per distinct byte sequence
//
// The interpreter's decode cache is process-wide and thread-local, keyed by
// `(rip, mem_generation)`, and every test CPU here starts at generation 0 — so
// the *first* decode of a given `rip` in the process is what that `rip` keeps
// meaning for the rest of the run. Two tests that hand different bytes to the
// same VA make the second one silently execute the first one's instruction:
// a wrong-execution failure, with no panic at the reuse site and no compiler
// diagnostic.
//
// [`alloc_code`] and [`alloc_code_page`] are the answer: each call returns a VA
// that no earlier call returned, so "I forgot to give this encoding its own
// address" cannot happen. Prefer them; they make the correct thing the easy
// thing.
//
// [`map_code_page`] is for the call sites whose base is a `const` shared with
// the assertions around them (`simd_dual`'s `SIMD_BASE`, the `code_va(id)`
// scheme in `btx_tests` / `implicit_*` / `locked_alu_tests`). It maps a
// caller-chosen address and therefore carries no freshness guarantee — with it
// the rule above is still on you. `cmpxchg_tests`' `cmpxchg_dual` maps its own
// page because its base is a parameter of a differential helper.

use super::*;
use crate::IcedCpu;
use crate::ThreadContext;
use crate::regs::Rflags;
use std::sync::atomic::{AtomicU64, Ordering};

// ── Engine plumbing ─────────────────────────────────────────────────────

pub(super) enum Engine {
    Iced(Box<IcedCpu>),
    Jit(Box<JitCpu>),
}

impl Engine {
    pub(super) fn cpu(&mut self) -> &mut dyn CpuEngine {
        match self {
            Self::Iced(c) => c.as_mut(),
            Self::Jit(c) => c.as_mut(),
        }
    }

    /// Execute enough to retire the single instruction under test.
    pub(super) fn run_rmw(&mut self) -> StepResult {
        match self {
            Self::Iced(c) => c.step_once_result().expect("iced step"),
            Self::Jit(c) => {
                let (result, _retired) = c.step_one().expect("jit step");
                result
            }
        }
    }

    pub(super) fn set_state(&mut self, ctx: &ThreadContext) {
        self.cpu().restore_thread_context(ctx);
        // A real guest thread switch does the same TLB/chain drop; without it a
        // JIT engine would reuse the previous trial's translated pointers.
        self.cpu().on_thread_switch();
    }

    pub(super) fn rax(&mut self) -> u64 {
        self.cpu().read_rax().expect("read rax")
    }

    /// ZF after the instruction under test (CMPXCHG is the one that sets it).
    pub(super) fn zf(&mut self) -> bool {
        let rflags = self.cpu().snapshot_thread_context().rflags;
        (rflags & Rflags::ZF) == Rflags::ZF
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Backend {
    Iced,
    Jit,
}

pub(super) const BACKENDS: [Backend; 2] = [Backend::Iced, Backend::Jit];

pub(super) fn open(backend: Backend) -> Engine {
    match backend {
        Backend::Iced => Engine::Iced(Box::new(IcedCpu::open_x86_64())),
        Backend::Jit => Engine::Jit(Box::new(JitCpu::open_x86_64())),
    }
}

/// A fresh per-thread engine over the *same* guest memory — the 1:1 model the
/// runtime uses for guest threads (`IcedCpu::new_standalone_with_mem` /
/// `JitCpu::new_shared`), and the reason two guest threads can race at all.
pub(super) fn open_worker(primary: &Engine) -> Engine {
    match primary {
        Engine::Iced(c) => Engine::Iced(Box::new(IcedCpu::new_standalone_with_mem(Arc::clone(
            c.guest_mem_arc(),
        )))),
        Engine::Jit(c) => Engine::Jit(Box::new(JitCpu::new_shared(Arc::clone(c.shared_jit())))),
    }
}

// ── Code pages ──────────────────────────────────────────────────────────

/// First VA [`alloc_code_page`] hands out. Clear of every base the tests pin
/// down by hand: the `0x1000_0000`–`0x10c0_0000` region, `0x1280_0000`,
/// `0x1400_0000`/`0x1500_0000`, `0x2000_0000`/`0x2001_0000`, `0x3000_0000`,
/// `0x4000_0000`–`0x40a0_0000`, and the TEB pages at `0x7000_0040_C000` /
/// `0x7EFD_0000`.
const FRESH_CODE_BASE: u64 = 0x5000_0000;

/// `MEM_RESERVE` rounds the base *and* the size to the Windows allocation
/// granularity (64 KiB), so consecutive pages are one granularity apart and
/// cannot overlap.
const FRESH_CODE_STRIDE: u64 = 0x1_0000;

/// Next free slot, process-wide: parallel test threads must not agree on one.
static NEXT_CODE_PAGE: AtomicU64 = AtomicU64::new(0);

/// Map a fresh, empty, executable page and return its guest VA.
///
/// Every call in the process returns a different VA. That is the whole point —
/// see the module comment: a reused VA silently makes the second encoding at
/// that address execute the first one's instruction.
pub(super) fn alloc_code_page(cpu: &mut dyn CpuEngine) -> u64 {
    let slot = NEXT_CODE_PAGE.fetch_add(1, Ordering::Relaxed);
    let base = FRESH_CODE_BASE + slot * FRESH_CODE_STRIDE;
    cpu.virtual_alloc(
        base,
        0x1000,
        MEM_RESERVE | MEM_COMMIT,
        protect::PAGE_EXECUTE_READWRITE,
    )
    .expect("alloc code page");
    base
}

/// Map a fresh executable page, write `bytes` at its start, return its VA.
///
/// This is the default way to run a byte sequence in these tests: no address to
/// choose, so nothing to get wrong.
pub(super) fn alloc_code(cpu: &mut dyn CpuEngine, bytes: &[u8]) -> u64 {
    let base = alloc_code_page(cpu);
    cpu.mem_write(base, bytes).expect("write code");
    base
}

/// Map a fresh *multi-page* executable region and return its guest VA.
///
/// [`alloc_code_page`] for a fixture longer than a page, or one that reads a
/// RIP-relative constant past the code: the whole slot is reserved in a single
/// call, because a second `MEM_RESERVE` inside an existing reservation is
/// "reserve over non-free page". `size` is rounded up to the 64 KiB granularity,
/// so the region and the next [`alloc_code_page`] slot cannot overlap.
pub(super) fn alloc_code_region(cpu: &mut dyn CpuEngine, size: usize) -> u64 {
    assert!(
        size as u64 <= FRESH_CODE_STRIDE,
        "a region must fit in one slot ({FRESH_CODE_STRIDE:#x} bytes)"
    );
    let slot = NEXT_CODE_PAGE.fetch_add(1, Ordering::Relaxed);
    let base = FRESH_CODE_BASE + slot * FRESH_CODE_STRIDE;
    cpu.virtual_alloc(
        base,
        size,
        MEM_RESERVE | MEM_COMMIT,
        protect::PAGE_EXECUTE_READWRITE,
    )
    .expect("alloc code region");
    base
}

/// Map one executable page at a caller-chosen `base`.
///
/// For the call sites that keep a `const` base because their assertions and
/// register setup are written against it. No freshness guarantee: the
/// distinct-VA rule is the caller's to honour here.
pub(super) fn map_code_page(cpu: &mut dyn CpuEngine, base: u64) {
    cpu.virtual_alloc(
        base,
        0x1000,
        MEM_RESERVE | MEM_COMMIT,
        protect::PAGE_EXECUTE_READWRITE,
    )
    .expect("alloc code page");
}

/// Map `size` bytes of executable memory at a caller-chosen `base` — the
/// multi-page form of [`map_code_page`].
pub(super) fn map_code_region(cpu: &mut dyn CpuEngine, base: u64, size: usize) {
    cpu.virtual_alloc(
        base,
        size,
        MEM_RESERVE | MEM_COMMIT,
        protect::PAGE_EXECUTE_READWRITE,
    )
    .expect("alloc code region");
}
