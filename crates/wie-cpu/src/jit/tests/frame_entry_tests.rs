//! Frame identity across the two entry paths, and the per-run reset set.
//!
//! `JitCtx` is no longer a temporary built and torn down per dispatcher entry:
//! it is *the* per-engine frame, owned by [`PerThreadJitState`] and handed to
//! compiled code by pointer. That buys ~4.6 KB less struct copy per entry (and
//! `tlb` alone is 2.3 KB, which is why `_platform_memmove` showed up in the
//! guest-thread profile at all), but it moves two invariants from "obviously
//! true because the value is fresh" to "true because we say so, so test it":
//!
//! 1. **One frame, both paths.** A compiled block must observe exactly the same
//!    `JitCtx` whether the Rust dispatcher entered it or a chain edge called
//!    into it. With one frame there is no second copy that could disagree; this
//!    file is what would catch a future patch that reintroduces one (a
//!    dispatcher-only prelude, a per-path copy, a stale `RegFile` view).
//! 2. **Reset exactly the per-run fields, and only those.** `fault`, the dirty
//!    masks, `chain_depth`, `insn_acc`, `chain_hops`, `chain_store_ops`, the
//!    load/store counters and `mem_path` accumulate during a run and must start
//!    at zero again on the next entry. Everything else — `tlb`, `pins`,
//!    `sticky_*`, `edge_ic_*`, `shadow_*`, the hot page — must *survive* an
//!    entry, because it was already persisted across runs before the frame
//!    became permanent. Over-resetting is as wrong as under-resetting: it turns
//!    every entry back into a cold start, and it silently discards the shadow
//!    return stack and edge IC that chaining depends on.
//!
//! Both are checked the same way: run the same guest bytes down both entry
//! paths, and compare the resulting guest-visible state field by field —
//! including XMM and the shadow return stack, which is where this project's
//! worst frame bugs (notepad's CRLF `movd` zeros, Doom Retro's dropped `xmm`
//! def) actually lived.
//!
//! Fixture, one page, `A` two instructions ahead of `B`:
//!
//! ```text
//! A (+0x00): mov rax,0 ; jmp B          <- the chain edge under test
//! B (+0x20): movdqu xmm3,[r15]          <- xmm path
//!            movdqu [r15+0x10],xmm3     <- xmm write-through, observable in memory
//!            add rax,0x1234
//!            call C                     <- shadow push
//!            add rax,rbx
//!            jmp STOP
//! C (+0x50): inc rax ; ret              <- shadow pop check + chain back
//! ```

use super::harness::map_code_region;
use super::*;
use crate::exec::StepResult;
use crate::regs::Rflags;
use config::JitConfig;
use lower::{MAX_CHAIN_DEPTH, SHADOW_DEPTH};

/// Code + data page pair for this file (no other fixture uses this base).
const FRAME_BASE: u64 = 0x10b0_0000;
/// RWX second page: `[0x00,0x10)` is the xmm load source, `+0x800` the stack.
const FRAME_DATA: u64 = FRAME_BASE + 0x1000;
/// Where `B` jumps to end the run; the harness stops as soon as RIP reaches it.
const STOP_OFF: u64 = 0x100;
/// Block `B` — entered by the chain edge in the chained arm, and directly in
/// the dispatched arm.
const B_OFF: u64 = 0x20;
/// Return address `call C` pushes, i.e. the shadow-stack slot `ret` must pop.
const RET_ADDR_OFF: u64 = 0x36;
/// `A`'s `mov rax,0` + 0x1234 + `C`'s `inc` + `rbx`.
const EXPECTED_RAX: u64 = 0x1234 + 1 + 0x1000;
/// Seeded shadow depth. Non-zero on purpose: an implementation that clears the
/// frame's shadow state per entry reports 0 here instead of 5.
const SEEDED_SHADOW_SP: u64 = 5;
/// Untouched shadow slot that must keep its seeded value (catches a blanket
/// `shadow_ret = [0; _]` reset, which would still pass the `sp` check alone).
const SHADOW_SENTINEL_SLOT: usize = 9;
const SHADOW_SENTINEL: u64 = 0x9999_9999_9999_9999;
/// 16 bytes loaded into `xmm3`, stored back out to `FRAME_DATA + 0x10`.
const XMM_PATTERN: [u8; 16] = [
    0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0x0f, 0xf0,
];

/// The guest bytes above, laid out so every terminator's target is a real
/// block start (no jump-to-next, which the decoder would fold into a single
/// block and make the chain edge disappear).
fn frame_fixture() -> Vec<u8> {
    let mut code = vec![0x90_u8; usize::try_from(STOP_OFF).unwrap_or(0x100) + 2];
    // A
    code[0x00..0x07].copy_from_slice(&[0x48, 0xc7, 0xc0, 0, 0, 0, 0]); // mov rax, 0
    code[0x07..0x09].copy_from_slice(&[0xeb, 0x17]); // jmp +0x20 (B)
    // B
    code[0x20..0x25].copy_from_slice(&[0xf3, 0x41, 0x0f, 0x6f, 0x1f]); // movdqu xmm3, [r15]
    code[0x25..0x2b].copy_from_slice(&[0xf3, 0x41, 0x0f, 0x7f, 0x5f, 0x10]); // movdqu [r15+0x10], xmm3
    code[0x2b..0x31].copy_from_slice(&[0x48, 0x05, 0x34, 0x12, 0x00, 0x00]); // add rax, 0x1234
    code[0x31..0x36].copy_from_slice(&[0xe8, 0x1a, 0x00, 0x00, 0x00]); // call +0x50 (C)
    code[0x36..0x39].copy_from_slice(&[0x48, 0x01, 0xd8]); // add rax, rbx
    code[0x39..0x3e].copy_from_slice(&[0xe9, 0xc2, 0x00, 0x00, 0x00]); // jmp +0x100 (STOP)
    // C
    code[0x50..0x53].copy_from_slice(&[0x48, 0xff, 0xc0]); // inc rax
    code[0x53] = 0xc3; // ret
    // STOP: never executed, the dispatcher stops when RIP reaches it.
    let stop = usize::try_from(STOP_OFF).unwrap_or(0x100);
    code[stop..stop + 2].copy_from_slice(&[0x0f, 0x0b]); // ud2
    code
}

/// What the invariant is about: everything a guest-visible result depends on,
/// plus the frame state the two paths must agree on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct FrameView {
    gpr: [u64; 16],
    rflags: u64,
    rip: u64,
    xmm: [u128; 16],
    shadow_sp: u64,
    shadow_ret: [u64; SHADOW_DEPTH],
    /// The guest-memory pointer the block was handed: captured once per engine
    /// at construction and re-stamped from the frame on every entry, so both
    /// entry paths must see the engine's canonical address (a stale one here
    /// means a chained block reads guest memory through a dead pointer).
    mem: usize,
    /// Chain edges taken by this arm — compared separately, never for equality
    /// across arms (that is the point of the positive control).
    hops: u64,
}

impl FrameView {
    fn take(cpu: &JitCpu) -> Self {
        let regs = &cpu.thread.regs;
        let ctx = &cpu.thread.ctx;
        Self {
            gpr: std::array::from_fn(|i| regs.gpr(i)),
            rflags: u64::from(regs.rflags),
            rip: regs.rip,
            xmm: std::array::from_fn(|i| ctx.xmm[i].to_u128()),
            shadow_sp: ctx.shadow_sp,
            shadow_ret: ctx.shadow_ret,
            mem: ctx.mem as usize,
            hops: cpu.stats().chain.hops,
        }
    }

    /// Assert the two arms observed the same frame. Reports the first field
    /// that differs, with the arm names, so a failure names the regression
    /// instead of dumping two 4 KB structs.
    fn assert_same(&self, other: &Self, a: &str, b: &str) {
        for i in 0..16 {
            assert_eq!(
                self.gpr[i], other.gpr[i],
                "{a} vs {b}: gpr[{i}] differs ({:#x} vs {:#x})",
                self.gpr[i], other.gpr[i]
            );
        }
        assert_eq!(self.rflags, other.rflags, "{a} vs {b}: rflags");
        assert_eq!(self.rip, other.rip, "{a} vs {b}: rip");
        for i in 0..16 {
            assert_eq!(
                self.xmm[i], other.xmm[i],
                "{a} vs {b}: xmm[{i}] differs ({:#034x} vs {:#034x})",
                self.xmm[i], other.xmm[i]
            );
        }
        assert_eq!(self.shadow_sp, other.shadow_sp, "{a} vs {b}: shadow_sp");
        assert_eq!(self.shadow_ret, other.shadow_ret, "{a} vs {b}: shadow_ret");
        assert_eq!(
            self.mem, other.mem,
            "{a} vs {b}: the guest-memory pointer differs ({:#x} vs {:#x})",
            self.mem, other.mem
        );
    }
}

/// Everything the dispatcher must restore before handing the frame out again,
/// plus the guest state, for one arm.
fn seed(cpu: &mut JitCpu) {
    let regs = &mut cpu.thread.regs;
    for i in 0..16 {
        regs.set_gpr(i, 0);
    }
    regs.set_gpr(15, FRAME_DATA); // r15: xmm load/store base
    regs.set_gpr(3, 0x1000); // rbx: read-only live-in for `add rax,rbx`
    regs.set_rsp(FRAME_DATA + 0x800);
    regs.set_rflags_checked(Rflags::from(0x202_u64));
    let ctx = &mut cpu.thread.ctx;
    ctx.shadow_sp = SEEDED_SHADOW_SP;
    ctx.shadow_ret = [0xAAAA_5555_AAAA_5555; SHADOW_DEPTH];
    ctx.shadow_ret[SHADOW_SENTINEL_SLOT] = SHADOW_SENTINEL;
}

/// Make the previous run's leftovers maximally visible. If the dispatcher does
/// not reset one of these, the next arm reports a fault, an inflated retired
/// count or a truncated chain instead of the clean result below.
fn poison(cpu: &mut JitCpu) {
    let ctx = &mut cpu.thread.ctx;
    ctx.fault = 1;
    ctx.fault_addr = 0xdead_0000;
    ctx.fault_size = 8;
    ctx.fault_access = 1;
    ctx.gpr_dirty_bits = 0xffff;
    ctx.xmm_dirty_bits = 0xffff;
    ctx.load_calls = 9;
    ctx.store_calls = 9;
    ctx.mem_path.slow = 9;
    ctx.chain_depth = MAX_CHAIN_DEPTH;
    ctx.insn_acc = 12_345;
    ctx.chain_hops = 777;
    ctx.chain_store_ops = 777;
}

/// Run one arm from `entry_rip` and return what it observed. `hops` is this
/// arm's own chain-edge count (a delta), not the engine's running total.
fn arm(cpu: &mut JitCpu, entry_rip: u64) -> FrameView {
    seed(cpu);
    cpu.write_rip(entry_rip).expect("arm rip");
    let hops_before = cpu.stats().chain.hops;
    let stop = FRAME_BASE + STOP_OFF;
    let mut steps = 0_usize;
    let mut retired_total = 0_usize;
    while cpu.thread.regs.rip < stop {
        let (result, retired) = cpu.step_one().expect("arm step");
        assert!(
            matches!(result, StepResult::Continue),
            "arm at {entry_rip:#x} ended with {result:?} after {steps} steps \
             (a leftover `fault` in the persistent frame?)"
        );
        retired_total = retired_total.saturating_add(retired);
        steps = steps.saturating_add(1);
        assert!(steps < 32, "arm at {entry_rip:#x} failed to reach STOP");
    }
    // `insn_acc` is per-run and poisons to 12 345: an unreset accumulator shows
    // up here as a retired count nobody could have executed.
    assert!(
        (1..64).contains(&retired_total),
        "arm at {entry_rip:#x} reported {retired_total} retired instructions for \
         a {steps}-step fixture (stale `insn_acc` in the persistent frame?)"
    );
    let mut view = FrameView::take(cpu);
    view.hops = view.hops.saturating_sub(hops_before);
    let canonical = cpu.thread.guest_mem as usize;
    assert_ne!(canonical, 0, "the engine must be bound to guest memory");
    assert_eq!(
        view.mem, canonical,
        "arm at {entry_rip:#x}: the frame must carry the engine's canonical \
         guest-memory pointer ({canonical:#x}), not {:#x}",
        view.mem
    );
    // Per-arm guest-visible results. These fail if the frame reached the block
    // with the wrong contents, whichever path delivered control.
    assert_eq!(
        view.gpr[0], EXPECTED_RAX,
        "arm at {entry_rip:#x}: rax must be {EXPECTED_RAX:#x}"
    );
    assert_eq!(view.rip, stop, "arm at {entry_rip:#x}: must stop at STOP");
    let want = u128::from_le_bytes(XMM_PATTERN);
    assert_eq!(
        view.xmm[3], want,
        "arm at {entry_rip:#x}: xmm3 must hold the loaded pattern"
    );
    let mut stored = [0_u8; 16];
    cpu.mem_read(FRAME_DATA + 0x10, &mut stored)
        .expect("read stored xmm");
    assert_eq!(
        stored, XMM_PATTERN,
        "arm at {entry_rip:#x}: `movdqu [r15+0x10],xmm3` must land in memory"
    );
    assert_eq!(
        view.shadow_sp, SEEDED_SHADOW_SP,
        "arm at {entry_rip:#x}: the call/ret pair must leave the seeded shadow \
         depth intact — a reset here means the dispatcher is clearing state it \
         must preserve"
    );
    assert_eq!(
        view.shadow_ret[usize::try_from(SEEDED_SHADOW_SP).unwrap_or(0)],
        FRAME_BASE + RET_ADDR_OFF,
        "arm at {entry_rip:#x}: the shadow slot the `call` pushed must hold the \
         return address"
    );
    assert_eq!(
        view.shadow_ret[SHADOW_SENTINEL_SLOT], SHADOW_SENTINEL,
        "arm at {entry_rip:#x}: an untouched shadow slot must keep its seeded \
         value — the frame must persist, not be rebuilt"
    );
    view
}

/// A block entered from a chain edge and the same block entered from the
/// dispatcher must observe the same frame, and the frame's per-run fields must
/// be reset between entries — no more and no less.
#[test]
fn chain_edge_and_dispatcher_entry_observe_the_same_frame() {
    let mut cpu = JitCpu::open_x86_64();
    map_code_region(&mut cpu, FRAME_BASE, 0x2000);
    cpu.mem_write(FRAME_BASE, &frame_fixture())
        .expect("write frame fixture");
    cpu.mem_write(FRAME_DATA, &XMM_PATTERN)
        .expect("write xmm pattern");

    let a = FRAME_BASE;
    let b = FRAME_BASE + B_OFF;

    // Warm-up: one pass from A compiles A, B, C and the `ret` landing block,
    // and populates this thread's chain table with all of them.
    let _ = arm(&mut cpu, a);
    assert!(
        cpu.has_ready_at(a) && cpu.has_ready_at(b),
        "warm-up must leave both A ({a:#x}) and B ({b:#x}) compiled"
    );

    // The measurement: chained, dispatched, then both again after poisoning the
    // frame's per-run fields.
    let chained = arm(&mut cpu, a);
    let dispatched = arm(&mut cpu, b);
    poison(&mut cpu);
    let dispatched_after_poison = arm(&mut cpu, b);
    poison(&mut cpu);
    let chained_after_poison = arm(&mut cpu, a);

    chained.assert_same(&dispatched, "chained", "dispatched");
    dispatched.assert_same(&dispatched_after_poison, "dispatched", "dispatched+poison");
    chained.assert_same(&chained_after_poison, "chained", "chained+poison");

    // Positive control: without this, the equality above could hold while the
    // chained arm never actually took a chain edge (e.g. A silently reached B
    // through the dispatcher). Entering at A must cost at least one native
    // block-to-block transfer more than entering at B directly.
    if JitConfig::get().chain_enabled() {
        // A is one extra block in front of B, so its arm takes exactly one
        // extra native transfer. Anything else means A did not reach B through
        // a chain edge at all and the equality above proved nothing.
        assert!(
            chained.hops > dispatched.hops,
            "entering at A must take the A->B chain edge: {} hops chained vs {} \
             dispatched (per arm, not cumulative)",
            chained.hops,
            dispatched.hops
        );
        assert_eq!(
            chained.hops,
            dispatched.hops.saturating_add(1),
            "A adds exactly one edge in front of B: {} vs {}",
            chained.hops,
            dispatched.hops
        );
        assert!(
            chained_after_poison.hops > dispatched.hops,
            "the A->B chain edge must survive the per-run reset: {} hops vs {}",
            chained_after_poison.hops,
            dispatched.hops
        );
    }
}
