//! Chain-edge counters (`JitStats::chain.hops` / `chain.store_ops`).
//!
//! `chain.hops` counts native transfers from one compiled block into a
//! compiled successor; `chain.store_ops` counts the GPR slots their
//! `writeback_gprs` flushed. They exist because nothing else measured the
//! per-block GPR round-trip: `jit_chain avg_width` reports chain-TABLE
//! population, not hit rate, and `exec.jit_insns` measures execution volume,
//! which a self-looping block produces without ever leaving native code.
//!
//! What these tests pin:
//! - both counters start at zero (a counter that starts non-zero silently
//!   poisons any ratio computed from it),
//! - they only ever grow,
//! - a **self-loop reports zero hops** — the property that separates this
//!   counter from `exec.jit_insns` and the reason `micro-exes/long_loop` is
//!   structurally blind to chain-edge work,
//! - a chain of distinct blocks reports hops, with a store count that stays
//!   inside the register file's bounds.

use super::*;
use crate::exec::StepResult;
use crate::mem::protect;
use crate::mem::{MEM_COMMIT, MEM_RESERVE};

/// Guest code region for these tests (unique per test-file convention).
const HOP_BASE: u64 = 0x10a0_0000;

/// A fresh engine has taken no chain edges. Any non-zero value here would
/// make every downstream ratio wrong from the first instruction.
#[test]
fn chain_counters_start_at_zero() {
    let cpu = JitCpu::open_x86_64();
    let s = cpu.stats();
    assert_eq!(s.chain.hops, 0, "chain_hops must start at zero");
    assert_eq!(s.chain.store_ops, 0, "chain_store_ops must start at zero");
}

/// A `dec rcx; jne` self-loop — the `micro-exes/long_loop` shape — retires
/// millions of instructions and takes **zero** chain hops, because the JIT
/// lowers it as a self-loop that stays in SSA registers. This is the load-
/// bearing assertion: if `hops` ever tracked execution volume instead of
/// native block-to-block transfers, the counter could not be used to argue
/// anything about chain-edge cost.
#[test]
fn self_loop_retires_instructions_without_chain_hops() {
    /// Iterations; 2^22 * 2 insns = 8,388,608 retired instructions.
    const ITERS: u64 = 1 << 22;

    let mut cpu = JitCpu::open_x86_64();
    cpu.virtual_alloc(
        HOP_BASE,
        0x1000,
        MEM_RESERVE | MEM_COMMIT,
        protect::PAGE_EXECUTE_READWRITE,
    )
    .expect("alloc");
    cpu.mem_write(HOP_BASE, &[0xff, 0xc9, 0x75, 0xfc])
        .expect("write loop");
    cpu.thread.regs.set_gpr(1, ITERS);
    cpu.write_rip(HOP_BASE).expect("rip");

    let mut steps = 0_usize;
    while cpu.thread.regs.rip < HOP_BASE.saturating_add(4) {
        let (result, _retired) = cpu.step_one().expect("step");
        assert!(
            matches!(result, StepResult::Continue),
            "self-loop must keep running (step {steps}, rip {:#x})",
            cpu.thread.regs.rip
        );
        steps = steps.saturating_add(1);
        assert!(steps < 1_000_000, "self-loop failed to terminate");
    }

    let s = cpu.stats();
    assert_eq!(
        cpu.thread.regs.gpr(1),
        0,
        "the loop must count down to zero"
    );
    assert!(
        s.exec.jit_insns > 1_000_000,
        "the loop must actually run in compiled code (retired {})",
        s.exec.jit_insns
    );
    assert_eq!(
        s.chain.hops, 0,
        "a self-loop never transfers to a successor block: hops={} \
         store_ops={}",
        s.chain.hops, s.chain.store_ops
    );
    assert_eq!(
        s.chain.store_ops, 0,
        "no chain edge means no chain-edge writeback: store_ops={}",
        s.chain.store_ops
    );
}

/// Two distinct blocks joined by `jmp`, with the loop counter in the second,
/// so every iteration crosses exactly two chain edges:
///
/// ```text
/// A (+0x00): add rax,rbx ; jmp B          <- rbx is a read-only live-in
/// B (+0x0f): add rax,rbx ; dec rcx ; jnz A
/// ```
///
/// `jmp`'s target is deliberately not its fallthrough (the 10 padding bytes):
/// the block decoder folds a jump-to-next into a fallthrough and keeps
/// decoding, which would merge A and B into one block and make this test
/// vacuous.
const RING_CODE: [u8; 25] = [
    0x48, 0x01, 0xd8, // +0x00 add rax,rbx
    0xeb, 0x0a, // +0x03 jmp +0x0f
    0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, // +0x05 padding
    0x48, 0x01, 0xd8, // +0x0f add rax,rbx
    0x48, 0xff, 0xc9, // +0x12 dec rcx
    0x75, 0xe9, // +0x15 jnz +0x00
    0x0f, 0x0b, // +0x17 ud2 (loop-exit stop)
];

/// A ring of distinct blocks must report hops, and the store count per hop
/// must stay inside the register file: at least the registers the edge
/// actually wrote, at most all sixteen.
#[test]
fn chained_blocks_report_hops_and_bounded_store_ops() {
    /// Iterations. Large enough that the promotion warmup (a few hundred
    /// interpreted visits per block) is a rounding error against the total.
    const ITERS: u64 = 20_000;

    let mut cpu = JitCpu::open_x86_64();
    cpu.virtual_alloc(
        HOP_BASE,
        0x1000,
        MEM_RESERVE | MEM_COMMIT,
        protect::PAGE_EXECUTE_READWRITE,
    )
    .expect("alloc");
    cpu.mem_write(HOP_BASE, &RING_CODE).expect("write ring");
    cpu.thread.regs.set_gpr(1, ITERS);
    cpu.thread.regs.set_gpr(3, 1); // rbx
    cpu.write_rip(HOP_BASE).expect("rip");

    let mut steps = 0_usize;
    while cpu.thread.regs.rip < HOP_BASE.saturating_add(0x17) {
        let Ok((_result, _retired)) = cpu.step_one() else {
            break; // ud2 at the loop exit is the intended stop
        };
        steps = steps.saturating_add(1);
        assert!(steps < 1_000_000, "ring failed to terminate");
    }
    assert_eq!(
        cpu.thread.regs.gpr(1),
        0,
        "the ring must count rcx down to zero"
    );

    // Register handoff across 39k chain edges: both blocks do `add rax,rbx`
    // with rbx = 1, so rax must be exactly 2 * ITERS at the end. A chain edge
    // that dropped or reordered a register writeback shows up here.
    assert_eq!(
        cpu.thread.regs.gpr(0),
        2 * ITERS,
        "chain hops must carry register state across every edge"
    );
    let s = cpu.stats();
    // Two edges per iteration, minus the warmup that ran interpreted.
    assert!(
        s.chain.hops > ITERS,
        "a two-block ring over {ITERS} iterations must report chain hops, got {}",
        s.chain.hops
    );
    assert!(
        s.chain.hops <= 2 * ITERS,
        "no more than two edges per iteration: {} hops for {ITERS} iterations",
        s.chain.hops
    );
    // Both blocks write `rax`; both read `rbx`, which the current predicate
    // re-stores because it is a read-only live-in. So at least one GPR store
    // per hop, and never more than the whole register file.
    assert!(
        s.chain.store_ops >= s.chain.hops,
        "every hop stores at least rax: {} ops for {} hops",
        s.chain.store_ops,
        s.chain.hops
    );
    assert!(
        s.chain.store_ops <= 16 * s.chain.hops,
        "a hop cannot store more than 16 GPRs: {} ops for {} hops",
        s.chain.store_ops,
        s.chain.hops
    );
    // The chain hop count is the volume measure for chain-edge work; it is
    // NOT derivable from `exec.jit_insns`. (That counter folds only from a
    // block's shared exit block, so a chain leaving at the MAX_CHAIN_DEPTH cap
    // — which is every ~48 hops here — loses its retired count. Pre-existing,
    // unrelated to these counters, but the reason this test asserts on hops.)
    assert!(
        s.chain.hops > ITERS && s.chain.store_ops > s.chain.hops,
        "chain-edge volume must dominate: {} hops, {} ops",
        s.chain.hops,
        s.chain.store_ops
    );
}

/// Both counters are monotonic: a snapshot taken part-way through a run never
/// reports fewer hops (or fewer stores) than an earlier snapshot. Generated
/// code accumulates into `JitCtx` and `run_compiled` folds it into the
/// per-thread snapshot, so a lost frame would show up here as a decrease.
#[test]
fn chain_counters_are_monotonic_across_a_run() {
    let mut cpu = JitCpu::open_x86_64();
    cpu.virtual_alloc(
        HOP_BASE,
        0x1000,
        MEM_RESERVE | MEM_COMMIT,
        protect::PAGE_EXECUTE_READWRITE,
    )
    .expect("alloc");
    cpu.mem_write(HOP_BASE, &RING_CODE).expect("write ring");
    cpu.thread.regs.set_gpr(1, 50_000);
    cpu.write_rip(HOP_BASE).expect("rip");

    let mut prev_hops = 0_u64;
    let mut prev_ops = 0_u64;
    let mut steps = 0_usize;
    while cpu.thread.regs.rip < HOP_BASE.saturating_add(0x17) && steps < 200_000 {
        if cpu.step_one().is_err() {
            break;
        }
        steps = steps.saturating_add(1);
        if !steps.is_multiple_of(64) {
            continue;
        }
        let s = cpu.stats();
        assert!(
            s.chain.hops >= prev_hops,
            "chain_hops went backwards: {prev_hops} -> {}",
            s.chain.hops
        );
        assert!(
            s.chain.store_ops >= prev_ops,
            "chain_store_ops went backwards: {prev_ops} -> {}",
            s.chain.store_ops
        );
        prev_hops = s.chain.hops;
        prev_ops = s.chain.store_ops;
    }
    assert!(
        prev_hops > 0,
        "the ring must have produced chain hops before the run ended"
    );
}

/// Cross-engine aggregation must add both counters. `merge_engine` skips only
/// the shared-derived fields, so a new per-thread counter that is not added
/// there would silently report the primary engine's volume only.
#[test]
fn chain_counters_merge_across_engines() {
    let shared = Arc::new(JitShared::new());
    let primary = JitCpu::new_shared(Arc::clone(&shared));
    let mut agg = primary.stats();
    agg.chain.hops = 5;
    agg.chain.store_ops = 9;

    let mut other = JitCpu::new_shared(Arc::clone(&shared));
    other.stats.chain.hops = 7;
    other.stats.chain.store_ops = 11;
    agg.merge_engine(&other.stats());

    assert_eq!(agg.chain.hops, 12, "hops must add across engines");
    assert_eq!(agg.chain.store_ops, 20, "store_ops must add across engines");
}
