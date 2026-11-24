//! Per-block opt-level tier-up: the shape-based earn signal, the
//! one-decision-per-VA memo, and the SAME-TIER-ONLY direct-chaining invariant
//! that having two Cranelift modules makes load-bearing.
//!
//! # The hazard these tests exist for
//!
//! `cranelift_module::FuncId` indexes the function table of the module that
//! declared it, and `Module::declare_func_in_func` indexes *this* module's
//! table. So handing a `FuncId` from the other tier's module to a compile
//! either panics or silently names a different function and emits a call to
//! the wrong address. The invariant is that `JitShared::chain_map_for` offers
//! each module only its own ids; a cross-tier edge then falls back to the
//! late-bound chain hop / dispatcher, both already supported and tested
//! elsewhere.
//!
//! # The program
//!
//! Two blocks. The loop head is a block entry (so the block's terminator
//! branches back to its own entry, which is what the earn signal reads), and
//! its exit lands in a one-pass block that therefore lands in the other tier:
//!
//! ```text
//! 0x00  ff c9            dec ecx        ; LOOP — block entry, self-loop  (Speed)
//! 0x02  75 fc            jne 0x00       ;   taken -> self, not-taken -> 0x04
//! 0x04  b8 63 00 00 00   mov eax, 99    ; EXIT — one pass               (Base)
//! 0x09  e9 ..            jmp 0x1000_0000         ; unmapped: ends the run
//! ```
//!
//! The loop encoding is the one `trip_tests` already pins: `ff c9` is
//! `dec ecx` (in 64-bit mode `0x48`..`0x4f` are REX prefixes, not the 32-bit
//! `dec` forms).
//!
//! These tests assert the **shipped default**, so they fail — all four — under
//! `WIE_JIT_TIER=0`. That is intended: with the switch off there is one module,
//! one opt level, and no cross-tier edge to test. The tiering *policy* itself
//! (memo, budget, rejection downgrade) is covered by the env-independent unit
//! tests in `jit::tier`.

use super::*;

/// Block offsets in [`tier_program`].
const LOOP_OFF: u64 = 0x00;
const LOOP_LEN: u64 = 0x04;
const EXIT_OFF: u64 = 0x04;
/// First offset past the program; a rip at or beyond it ends the run.
const STOP_OFF: u64 = 0x0e;

/// Unmapped VA the exit block jumps to, so the run terminates deterministically
/// without a `NotPure` byte (an unsupported instruction would make the exit
/// block uncompilable, and an uncompiled block is not in the chain table at
/// all — there would then be no cross-tier edge to test).
const LEAVE_VA: u64 = 0x1000_0000;

/// Guest bytes for the program described in the module docs.
fn tier_program() -> Vec<u8> {
    let mut code: Vec<u8> = vec![
        0xff, 0xc9, // 0x00 dec ecx        (loop head = block entry)
        0x75, 0xfc, // 0x02 jne 0x00       (back edge to the block entry)
        0xb8, 0x63, 0x00, 0x00, 0x00, // 0x04 mov eax, 99
    ];
    let next = SIMD_BASE + STOP_OFF + u64::try_from(code.len()).unwrap_or(0);
    // `e9 rel32` — displacement computed in i32 space, no narrowing casts.
    // It must fit: a `jmp` beyond +/-2 GiB is not encodable as `e9 rel32` (it
    // becomes a far jump), which would silently change the block's shape.
    let disp =
        i32::try_from(LEAVE_VA.cast_signed() - next.cast_signed()).expect("rel32 target in range");
    code.push(0xe9);
    code.extend_from_slice(&disp.to_le_bytes());
    assert_eq!(code.len(), usize::try_from(STOP_OFF).unwrap_or(0));
    code
}

/// A JIT CPU with the tier program mapped, ready to run from its entry.
fn tier_cpu() -> JitCpu {
    let mut cpu = JitCpu::open_x86_64();
    cpu.virtual_alloc(
        SIMD_BASE,
        0x2000,
        MEM_RESERVE | MEM_COMMIT,
        protect::PAGE_EXECUTE_READWRITE,
    )
    .expect("alloc");
    cpu.mem_write(SIMD_BASE, &tier_program()).expect("code");
    cpu
}

/// Run the program from its entry to the end, leaving the final register file
/// in the engine. Re-runnable: resets the entry rip and the loop counter, so a
/// second run re-executes every block.
fn run_tier_program(cpu: &mut JitCpu) {
    cpu.thread.regs.set_gpr(1, 5); // ecx
    cpu.write_rip(SIMD_BASE).expect("rip");
    for _ in 0..64 {
        if cpu.read_rip().expect("rip read") >= SIMD_BASE + STOP_OFF {
            break;
        }
        // A step may end on an exception (the `jmp` target is unmapped) —
        // that is the stop, not a failure.
        let Ok((result, _retired)) = cpu.step_one() else {
            break;
        };
        if !matches!(result, StepResult::Continue) {
            break;
        }
    }
}

/// The tier tag the chain table holds for `rip` (i.e. the module that
/// declared its `FuncId`).
fn chain_tier(cpu: &JitCpu, off: u64) -> OptTier {
    cpu.shared
        .chain_ids
        .pin()
        .get(&(SIMD_BASE + off))
        .expect("chain id installed")
        .tier
}

/// The earn signal is block SHAPE: the self-loop tiers up and the one-pass
/// block it exits into does not.
#[test]
fn self_loop_tiers_up_and_its_exit_block_does_not() {
    let mut cpu = tier_cpu();
    run_tier_program(&mut cpu);

    assert_eq!(cpu.thread.regs.gpr(0), 99, "eax: exit block ran");
    assert_eq!(cpu.thread.regs.gpr(1), 0, "rcx: loop ran to zero");

    assert_eq!(chain_tier(&cpu, LOOP_OFF), OptTier::Speed, "self-loop");
    assert_eq!(chain_tier(&cpu, EXIT_OFF), OptTier::Base, "one-pass exit");

    // Exactly one tier-up, and it is the loop.
    let counters = cpu.shared.tier_counters();
    assert_eq!(counters.tier_ups, 1);
    assert_eq!(counters.tier_rejects, 0);
    assert!(
        counters.budget_left < super::tier::TIER_BUDGET_DEFAULT,
        "the tier-up must be charged to the budget at decision time"
    );
}

/// The invariant, end to end: with the loop recompiled while its successor
/// sits in the OTHER tier, the cross-tier edge is never direct-called, the
/// block still executes correctly through the fallback, and the recompile
/// reuses the memoised decision (no second tier-up, no oscillation).
#[test]
fn cross_tier_successor_edge_falls_back_and_still_executes() {
    let mut cpu = tier_cpu();
    run_tier_program(&mut cpu);
    assert_eq!(cpu.thread.regs.gpr(0), 99, "warm-up run");

    // Drop ONLY the loop block. The exit block stays Ready in the base
    // module's chain table, so the loop's recompile now sees a cross-tier
    // successor — the exact shape that would hand a foreign `FuncId` to
    // `declare_func_in_func`.
    cpu.invalidate_code_range(SIMD_BASE + LOOP_OFF, usize::try_from(LOOP_LEN).unwrap_or(5));
    assert!(!cpu.has_ready_at(SIMD_BASE + LOOP_OFF), "loop dropped");
    assert!(
        cpu.has_ready_at(SIMD_BASE + EXIT_OFF),
        "exit block must stay warm in the other tier"
    );
    assert_eq!(chain_tier(&cpu, EXIT_OFF), OptTier::Base);

    run_tier_program(&mut cpu);
    assert_eq!(cpu.thread.regs.gpr(0), 99, "cross-tier edge result");
    assert_eq!(cpu.thread.regs.gpr(1), 0, "cross-tier edge loop count");
    assert_eq!(
        chain_tier(&cpu, LOOP_OFF),
        OptTier::Speed,
        "the recompile kept its memoised tier"
    );
    assert_eq!(
        cpu.shared.tier_counters().tier_ups,
        1,
        "a recompile must not spend a second tier-up: the decision is memoised"
    );
}

/// The filter that makes the invariant hold, asserted directly: each module's
/// map holds its own ids and nothing else, so no cross-module `FuncId` can
/// reach `declare_func_in_func`.
#[test]
fn chain_map_is_partitioned_by_tier() {
    let mut cpu = tier_cpu();
    run_tier_program(&mut cpu);

    let base_ids = cpu.shared.chain_map_for(OptTier::Base);
    let speed_ids = cpu.shared.chain_map_for(OptTier::Speed);
    let loop_va = SIMD_BASE + LOOP_OFF;
    let exit_va = SIMD_BASE + EXIT_OFF;

    let loop_id = cpu
        .shared
        .chain_ids
        .pin()
        .get(&loop_va)
        .expect("loop chain id")
        .func_id;
    let exit_id = cpu
        .shared
        .chain_ids
        .pin()
        .get(&exit_va)
        .expect("exit chain id")
        .func_id;

    assert_eq!(speed_ids.get(&loop_va).copied(), Some(loop_id));
    assert_eq!(base_ids.get(&exit_va).copied(), Some(exit_id));
    assert!(
        !speed_ids.contains_key(&exit_va),
        "a base-module FuncId leaked into the tier module's direct-chain map"
    );
    assert!(
        !base_ids.contains_key(&loop_va),
        "a tier-module FuncId leaked into the base module's direct-chain map"
    );
    // Two distinct modules, so the partition is not vacuous: each tier's code
    // resolves inside the module that declared it.
    let (base_ptr, tier_ptr) = {
        let mut eng = cpu.shared.engine.lock().expect("engine lock");
        let eng = eng.as_mut().expect("engine");
        let tier = eng.tier.as_mut().expect("tier module");
        let base = &eng.base;
        (
            base.module.get_finalized_function(exit_id),
            tier.module.get_finalized_function(loop_id),
        )
    };
    assert!(!base_ptr.is_null() && !tier_ptr.is_null());
    assert_ne!(
        base_ptr, tier_ptr,
        "two opt levels must be two modules, not one"
    );
}

/// Repeated entry is NOT an earn signal: ten executions re-enter the exit
/// block ten times and it never tiers up. This is the 7-Zip guardrail — a
/// block that runs once (or a few times) must never pay `speed`.
#[test]
fn revisiting_a_one_pass_block_never_tiers_it_up() {
    let mut cpu = tier_cpu();
    for _ in 0..10 {
        run_tier_program(&mut cpu);
    }
    assert_eq!(cpu.thread.regs.gpr(0), 99, "last run still correct");
    assert_eq!(chain_tier(&cpu, EXIT_OFF), OptTier::Base, "exit");
    assert_eq!(chain_tier(&cpu, LOOP_OFF), OptTier::Speed, "loop");
    assert_eq!(
        cpu.shared.tier_counters().tier_ups,
        1,
        "ten runs, one tier-up decision: the memo is per VA, not per visit"
    );
}
