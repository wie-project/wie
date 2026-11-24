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

use super::harness::map_code_page;
use super::*;
use crate::exec::StepResult;
use lower::{MAX_CHAIN_DEPTH, chain_table_clear};

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
    map_code_page(&mut cpu, HOP_BASE);
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
    map_code_page(&mut cpu, HOP_BASE);
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
    map_code_page(&mut cpu, HOP_BASE);
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

/// Guest instructions retired by one iteration of [`RING_CODE`]: block A is
/// `add rax,rbx; jmp` (2) and block B is `add rax,rbx; dec rcx; jnz` (3).
const RING_INSNS_PER_ITER: u64 = 5;

/// Run [`RING_CODE`] for `iters` iterations on a fresh engine.
fn run_ring(iters: u64) -> JitCpu {
    let mut cpu = JitCpu::open_x86_64();
    map_code_page(&mut cpu, HOP_BASE);
    cpu.mem_write(HOP_BASE, &RING_CODE).expect("write ring");
    cpu.thread.regs.set_gpr(1, iters);
    cpu.thread.regs.set_gpr(3, 1); // rbx
    cpu.write_rip(HOP_BASE).expect("rip");

    let mut steps = 0_usize;
    while cpu.thread.regs.rip < HOP_BASE.saturating_add(0x17) {
        if cpu.step_one().is_err() {
            break; // ud2 at the loop exit is the intended stop
        }
        steps = steps.saturating_add(1);
        assert!(steps < 1_000_000, "ring failed to terminate");
    }
    cpu
}

/// Every guest instruction a chain retires must be charged, whichever side of
/// the JIT/iced boundary executed it.
///
/// This is the regression test for the `insn_acc` undercount: `TripCounter`'s
/// count was folded only from a block's shared `exit` block, so a block that
/// transferred to a successor — through any of `emit_chain_or_exit`'s three
/// call sites, or its depth-cap `return_` — dropped its retired count on the
/// floor. Only the last block of each native chain was ever charged, which made
/// `exec.jit_insns` (and the `insn_per_entry` ratio derived from it) report
/// single-digit instruction counts for chain-heavy code.
///
/// Ground truth is exact: 5 instructions per iteration, and the two counters
/// partition the work, so their sum must equal `5 * iters` with no slack. This
/// ring crosses the `MAX_CHAIN_DEPTH` cap every ~48 hops, so it covers the
/// depth-cap exit and all three post-call exits on every frame boundary.
#[test]
fn chained_blocks_retire_every_guest_instruction() {
    const ITERS: u64 = 20_000;
    let cpu = run_ring(ITERS);
    assert_eq!(
        cpu.thread.regs.gpr(1),
        0,
        "the ring must count rcx down to zero"
    );

    let s = cpu.stats();
    let retired = s.exec.jit_insns.saturating_add(s.exec.iced_insns);
    assert_eq!(
        retired,
        RING_INSNS_PER_ITER * ITERS,
        "chain must charge every retired instruction: jit={} iced={} hops={} \
         (expected {} = 5 insns x {ITERS} iterations)",
        s.exec.jit_insns,
        s.exec.iced_insns,
        s.chain.hops,
        RING_INSNS_PER_ITER * ITERS
    );
    // ...and the bulk of it must be on the compiled side specifically. A fix
    // that accidentally moved the charge onto the interpreted path would pass
    // the sum assertion above while still making `jit_insns` wrong.
    assert!(
        s.exec.jit_insns > retired / 2,
        "the ring must run mostly compiled: jit={} of {retired}",
        s.exec.jit_insns
    );
    // The chain-edge counters must stay consistent with the instruction count
    // they are supposed to describe: this ring crosses exactly two edges per
    // five retired instructions, once both blocks are compiled. Guards against a
    // future fix that makes the two families of counter drift apart — they are
    // accumulated at different points (before the transfer vs. on the way out
    // of the block), and only one of them was ever affected by the flush bug.
    let expected_hops = retired * 2 / 5;
    let hops = s.chain.hops;
    assert!(
        expected_hops / 10 <= hops && hops <= expected_hops,
        "chain_hops must track retired instructions: {hops} hops for \
         {retired} retired (expected ~{expected_hops} = 2 edges per 5 insns)"
    );
}

/// The cross-thread invalidation guard returns to the dispatcher from its own
/// `stale_blk`, bypassing the shared `exit` block — a second exit path with
/// the same undercount. Bumping `invalidate_gen` once the ring is chaining
/// forces it.
///
/// Without a flush on that path, whichever block observed the stale generation
/// silently lost its retired count. (The bump is raw rather than routed
/// through `invalidate_code_range` on purpose: the cache keeps its `Ready`
/// entries, so every later guard observation fires too and the ring advances
/// one block per dispatcher step. That makes this path the *only* exit in the
/// run, which is exactly what the assertion needs to isolate.)
#[test]
fn stale_generation_exit_still_retires_instructions() {
    const ITERS: u64 = 20_000;

    let mut cpu = JitCpu::open_x86_64();
    map_code_page(&mut cpu, HOP_BASE);
    cpu.mem_write(HOP_BASE, &RING_CODE).expect("write ring");
    cpu.thread.regs.set_gpr(1, ITERS);
    cpu.thread.regs.set_gpr(3, 1);
    cpu.write_rip(HOP_BASE).expect("rip");

    let mut bumped = false;
    let mut steps = 0_usize;
    while cpu.thread.regs.rip < HOP_BASE.saturating_add(0x17) {
        if cpu.step_one().is_err() {
            break;
        }
        steps = steps.saturating_add(1);
        // Bump on the first observed chain hop: the blocks are compiled and
        // chaining by then, so the guard is actually in the executed path.
        if !bumped && cpu.stats().chain.hops > 0 {
            cpu.shared
                .invalidate_gen
                .fetch_add(1, std::sync::atomic::Ordering::Release);
            bumped = true;
        }
        assert!(steps < 1_000_000, "ring failed to terminate");
    }
    assert!(bumped, "the generation must have been bumped mid-run");
    assert_eq!(
        cpu.thread.regs.gpr(1),
        0,
        "the ring must still count rcx down to zero after the purge"
    );

    let s = cpu.stats();
    let retired = s.exec.jit_insns.saturating_add(s.exec.iced_insns);
    assert_eq!(
        retired,
        RING_INSNS_PER_ITER * ITERS,
        "the stale-generation exit must charge its instructions like any \
         other: jit={} iced={} hops={} (expected {})",
        s.exec.jit_insns,
        s.exec.iced_insns,
        s.chain.hops,
        RING_INSNS_PER_ITER * ITERS
    );
}

// --- The emitted hop's successor resolution -------------------------------
//
// A hop resolves its successor in emitted code, from two inline tiers: the
// edge-IC branch cascade, then the chain table's first two probe slots (whose
// offsets `chain_slot_offset` hashes at compile time for a static target, and
// which the emitted code computes per hop for a dynamic one). Only when both
// miss does control leave emitted code for `wie_jit_chain_lookup`. These tests
// pin that split from the guest side: the successor really runs, the counters
// say the resolver was not reached, and every decline path still lands in the
// dispatcher with correct state.

/// A hop's successor must run **inside the emitted transfer**, without the
/// guest taking a dispatcher entry per block and without the host resolver
/// being called for steady-state traffic.
///
/// `JitCtx::chain_helper_calls` counts the hops that had to call
/// `wie_jit_chain_lookup`; `chain.hops` counts the hops that transferred to a
/// compiled successor. If the inline tiers stopped resolving, the first would
/// climb toward the second and the frame rate would pay a host call per edge —
/// exactly the regression this shape exists to prevent. The bound is a ratio
/// (1 in 1000), not zero, because the very first hops of a run legitimately
/// race a target's compile-time chain-table insert.
#[test]
fn chain_hop_resolves_inline_without_the_host_resolver() {
    const ITERS: u64 = 20_000;
    let cpu = run_ring(ITERS);
    let s = cpu.stats();
    let helper = cpu.thread.ctx.chain_helper_calls;
    assert!(
        s.chain.hops > ITERS,
        "the ring must take chain hops: {} hops for {ITERS} iterations",
        s.chain.hops
    );
    assert!(
        helper.saturating_mul(1000) <= s.chain.hops,
        "steady-state hops must resolve in emitted code: {helper} host-resolver \
         calls for {} hops ({} per hop)",
        s.chain.hops,
        helper as f64 / s.chain.hops.max(1) as f64
    );
    // ...and the successor really executed: both blocks of the ring write rax,
    // so the accumulated value proves every hop ran its target.
    assert_eq!(
        cpu.thread.regs.gpr(0),
        2 * ITERS,
        "every hop must run its successor"
    );
}

/// The **dynamic** target path: a `ret` whose return address is only known at
/// runtime still chains, folding `chain_hash` into the emitted probe instead of
/// baking an offset.
///
/// ```text
/// B (+0x00): add rax,rbx ; call A          <- static edge (baked offset)
/// B (+0x08): dec rcx ; jnz +0x00           <- A's return address is here
/// B (+0x0d): ud2                           <- loop exit
/// A (+0x10): add rax,rbx x3 ; ret          <- 4 insns: NOT a micro-stub, so
///                                             the `ret` is lowered by the JIT
///                                             and its chain target is dynamic
/// ```
///
/// Per iteration: one `add` in B and three in A, so `rax += 4*rbx`.
#[test]
fn chain_hop_with_a_dynamic_target_runs_the_successor() {
    /// Assembled by hand: `call` at +0x03 has next = +0x08 and target = +0x10
    /// (rel32 = 8); `jnz` at +0x0b has next = +0x0d and target = +0x00
    /// (rel8 = -13).
    const CODE: [u8; 0x1a] = [
        0x48, 0x01, 0xd8, // +0x00 add rax,rbx
        0xe8, 0x08, 0x00, 0x00, 0x00, // +0x03 call +0x10
        0x48, 0xff, 0xc9, // +0x08 dec rcx
        0x75, 0xf3, // +0x0b jnz +0x00
        0x0f, 0x0b, // +0x0d ud2 (loop-exit stop)
        0x90, // +0x0f padding
        0x48, 0x01, 0xd8, // +0x10 add rax,rbx
        0x48, 0x01, 0xd8, // +0x13 add rax,rbx
        0x48, 0x01, 0xd8, // +0x16 add rax,rbx
        0xc3, // +0x19 ret
    ];
    /// The page is RWX, so the upper half is a usable guest stack.
    const STACK_TOP: u64 = HOP_BASE + 0x800;
    const ITERS: u64 = 5_000;

    let mut cpu = JitCpu::open_x86_64();
    map_code_page(&mut cpu, HOP_BASE);
    cpu.mem_write(HOP_BASE, &CODE).expect("write call/ret ring");
    cpu.thread.regs.set_gpr(1, ITERS); // rcx: trip count
    cpu.thread.regs.set_gpr(3, 1); // rbx: the increment
    cpu.thread.regs.set_gpr(4, STACK_TOP); // rsp: the callee's `ret` pops here
    cpu.write_rip(HOP_BASE).expect("rip");

    // The ring exits when `dec rcx` falls through, so the trip counter is the
    // loop condition: one dispatcher entry can retire several iterations (the
    // edges chain), which is exactly what this fixture wants to exercise.
    let mut steps = 0_usize;
    while cpu.thread.regs.gpr(1) != 0 {
        if cpu.step_one().is_err() {
            break;
        }
        steps = steps.saturating_add(1);
        assert!(steps < 1_000_000, "call/ret ring failed to terminate");
    }
    assert_eq!(
        cpu.thread.regs.gpr(1),
        0,
        "the call/ret ring must run every iteration"
    );
    assert_eq!(
        cpu.thread.regs.gpr(0),
        4 * ITERS,
        "each iteration runs one `add` in the caller and three in the callee"
    );
    let s = cpu.stats();
    assert!(
        s.chain.hops > ITERS,
        "the call/ret ring must take chain hops through the dynamic target: {}",
        s.chain.hops
    );
    assert!(
        cpu.thread.ctx.chain_helper_calls.saturating_mul(20) <= s.chain.hops,
        "the inline hash must resolve most dynamic-target hops: {} host calls \
         for {} hops",
        cpu.thread.ctx.chain_helper_calls,
        s.chain.hops
    );
}

/// The **chain-table** tier on its own: with the edge IC wiped before every
/// entry, the branch cascade can never answer, so a hop that still resolves
/// natively must have come from the inline probe of
/// `chain_slots + chain_slot_offset(target)` (or its `+CHAIN_SLOT_BYTES`
/// neighbour) — never from the host resolver.
///
/// This is the test that pins the *baked offset*: if `chain_slot_offset` and the
/// emitted arithmetic ever disagree, tier 2 misses silently and every hop falls
/// through to `wie_jit_chain_lookup`. Hops alone would not notice (the resolver
/// still transfers); the resolver-call count is what does.
#[test]
fn chain_hop_resolves_from_the_chain_table_with_a_cold_edge_ic() {
    const ITERS: u64 = 2_000;

    let mut cpu = JitCpu::open_x86_64();
    map_code_page(&mut cpu, HOP_BASE);
    cpu.mem_write(HOP_BASE, &RING_CODE).expect("write ring");
    cpu.thread.regs.set_gpr(1, ITERS);
    cpu.thread.regs.set_gpr(3, 1);
    cpu.write_rip(HOP_BASE).expect("rip");

    let mut steps = 0_u64;
    while cpu.thread.regs.rip < HOP_BASE.saturating_add(0x17) {
        cpu.thread.ctx.edge_ic_va = [0; 4];
        cpu.thread.ctx.edge_ic_fn = [0; 4];
        if cpu.step_one().is_err() {
            break;
        }
        steps = steps.saturating_add(1);
        assert!(steps < 1_000_000, "ring failed to terminate");
    }
    assert_eq!(
        cpu.thread.regs.gpr(0),
        2 * ITERS,
        "the ring must still compute its recurrence"
    );
    let s = cpu.stats();
    let helper = cpu.thread.ctx.chain_helper_calls;
    // Two edges per iteration, minus the one edge per dispatcher entry that the
    // wipe itself costs (there are ~40 entries: the chain depth cap bounds a
    // frame at `MAX_CHAIN_DEPTH` hops).
    assert!(
        s.chain.hops >= 2 * ITERS - 200,
        "the inline chain-table probe must keep the ring chaining with a cold \
         edge IC: {} hops, expected ~{} (2 per iteration)",
        s.chain.hops,
        2 * ITERS
    );
    assert!(
        helper.saturating_mul(100) <= s.chain.hops,
        "the chain-table tier must resolve these hops, not the host resolver: \
         {helper} resolver calls for {} hops",
        s.chain.hops
    );
}

/// A hop that cannot resolve **declines**: it restores `chain_depth`, leaves RIP
/// on the successor and returns to the dispatcher, which runs the target as if
/// no chaining existed. Wiping both per-thread resolution caches before every
/// entry makes nearly every edge unresolvable, and the guest must still compute
/// exactly the same answer.
///
/// "Nearly" is the load-bearing word, and the assertions are written to it.
/// This is not a way to disable chaining: after a decline the dispatcher
/// re-enters and legitimately re-populates the caches from its code cache, so an
/// entry can still take one native transfer before the next wipe. What the
/// counter split shows is the shape of the run — the host resolver reached on
/// essentially every entry (the decline path really executed), at most ~1 native
/// transfer per entry instead of ~48, and no register or RIP state lost at any
/// of those declines.
#[test]
fn chain_hop_with_no_resolvable_successor_declines_to_the_dispatcher() {
    const ITERS: u64 = 2_000;

    let mut cpu = JitCpu::open_x86_64();
    map_code_page(&mut cpu, HOP_BASE);
    cpu.mem_write(HOP_BASE, &RING_CODE).expect("write ring");
    cpu.thread.regs.set_gpr(1, ITERS);
    cpu.thread.regs.set_gpr(3, 1);
    cpu.write_rip(HOP_BASE).expect("rip");

    let mut steps = 0_u64;
    while cpu.thread.regs.rip < HOP_BASE.saturating_add(0x17) {
        // Remove *every* inline resolution source before each entry: the chain
        // table (tier 2 and the host resolver) and the monomorphic edge IC
        // (tier 1). Both are per-thread caches of compiled successors, so
        // wiping them is exactly "no successor is resolvable this entry".
        chain_table_clear(cpu.thread.chain_slots.as_mut());
        cpu.thread.ctx.edge_ic_va = [0; 4];
        cpu.thread.ctx.edge_ic_fn = [0; 4];
        if cpu.step_one().is_err() {
            break;
        }
        steps = steps.saturating_add(1);
        assert!(steps < 1_000_000, "ring failed to terminate");
    }
    assert_eq!(
        cpu.thread.regs.gpr(0),
        2 * ITERS,
        "a declined hop must still run the successor, with its state intact"
    );
    assert_eq!(cpu.thread.regs.gpr(1), 0, "the ring must count rcx down");
    let helper = cpu.thread.ctx.chain_helper_calls;
    // One resolver call per entry, minus the entries whose edge the dispatcher
    // had already cached again: hence the small slack rather than equality.
    assert!(
        helper.saturating_add(4) >= steps,
        "the decline path must reach the host resolver on essentially every \
         entry: {helper} resolver calls for {steps} dispatcher entries"
    );
    assert!(
        cpu.stats().chain.hops <= steps.saturating_add(64),
        "clearing the caches must leave at most ~1 native transfer per entry: \
         {} hops over {steps} entries",
        cpu.stats().chain.hops
    );
}

/// The host-stack cap still declines the hop at `MAX_CHAIN_DEPTH`, and the
/// dispatcher re-entry at the cap preserves the register state the chain was
/// carrying.
///
/// The ring crosses two edges per iteration, so `MAX_CHAIN_DEPTH / 2`
/// iterations lands the cap exactly on a block boundary; the trailing `+1`
/// forces one more hop *after* a cap bounce, which is where a wrong depth
/// restore (forgetting to reload, or restoring the pre-increment value in the
/// wrong arm) would show up as a corrupted accumulator.
#[test]
fn chain_hop_at_the_depth_cap_preserves_state() {
    let iters = MAX_CHAIN_DEPTH + 1;
    let cpu = run_ring(iters);
    assert_eq!(
        cpu.thread.regs.gpr(0),
        2 * iters,
        "crossing the host-stack cap must not lose a register hand-off"
    );
    assert_eq!(cpu.thread.regs.gpr(1), 0, "the ring must count rcx down");
    // `run_ring` steps until the terminator: with a cap of MAX_CHAIN_DEPTH the
    // dispatcher must have been re-entered several times, so `chain_depth` is
    // back to zero at the end of every entry.
    assert!(
        cpu.thread.ctx.chain_depth == 0,
        "chain_depth must return to zero after a completed entry, got {}",
        cpu.thread.ctx.chain_depth
    );
}
