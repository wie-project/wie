//! Dynamic retired-instruction accounting (the Wave 4 coverage denominator).
//!
//! `JitStats::exec.jit_insns` used to charge a compiled block its **static**
//! decoded length once per block **entry** (`meta.insn_count`), so a
//! self-looping block undercounted by its trip factor: `micro-exes/long_loop`
//! reported `total=25` for ~3x10^8 retired instructions. The lowering step now
//! emits a loop-carried trip counter (`lower::emit::TripCounter`) that
//! accumulates into `JitCtx::insn_acc`, and `run_compiled` charges *that*.
//!
//! These tests pin the two halves of the contract:
//! - a tight loop reports a real dynamic count (the acceptance test that was
//!   impossible to write before the counter existed), and
//! - a block that does not loop is still charged exactly its static length,
//!   so the counter cannot inflate the denominator either.

use super::*;

/// Guest code region for these tests (unique per test-file convention).
const TRIP_BASE: u64 = 0x1090_0000;

/// The acceptance test: a tight guest loop must retire more than 10^7
/// instructions. Before the emitted trip counter this reported the block's
/// static length (2) times the handful of times the dispatcher entered it —
/// a number in the tens, no matter how long the loop ran.
#[test]
fn self_loop_reports_dynamic_retired_instructions() {
    /// Iterations. 2^23 * 2 insns = 16,777,216 retired instructions — an
    /// order of magnitude above the 10^7 acceptance threshold, while the loop
    /// still finishes in a few tens of milliseconds natively.
    const ITERS: u64 = 1 << 23;

    let mut cpu = JitCpu::open_x86_64();
    cpu.virtual_alloc(
        TRIP_BASE,
        0x1000,
        MEM_RESERVE | MEM_COMMIT,
        protect::PAGE_EXECUTE_READWRITE,
    )
    .expect("alloc");
    // `dec ecx` is `FF /1` in 64-bit mode (0x48–0x4f are REX prefixes there,
    // not the 32-bit `dec` encodings); `jne rel8(-4)` at +2 targets the block
    // entry, so the body is exactly the two decoded instructions (the
    // terminator is part of `insns`).
    cpu.mem_write(TRIP_BASE, &[0xff, 0xc9, 0x75, 0xfc])
        .expect("write loop");
    cpu.thread.regs.set_gpr(1, ITERS);
    cpu.write_rip(TRIP_BASE).expect("rip");

    let mut retired_total = 0_usize;
    let mut steps = 0_usize;
    loop {
        let (result, retired) = cpu.step_one().expect("step");
        assert!(
            matches!(result, StepResult::Continue),
            "loop body must keep running (step {steps}, result {result:?}, rip {:#x})",
            cpu.thread.regs.rip
        );
        retired_total = retired_total.saturating_add(retired);
        steps = steps.saturating_add(1);
        if cpu.thread.regs.rip >= TRIP_BASE.saturating_add(4) {
            break;
        }
        assert!(
            steps < 1_000_000,
            "the loop must terminate; it ran {steps} dispatcher steps with rcx={}",
            cpu.thread.regs.gpr(1)
        );
    }
    assert_eq!(
        cpu.thread.regs.gpr(1),
        0,
        "the loop must count down to zero"
    );

    let stats = cpu.stats();
    assert!(
        stats.exec.jit_insns > 10_000_000,
        "a tight loop retired {} instructions, not a block-entry count \
         (iced={})",
        stats.exec.jit_insns,
        stats.exec.iced_insns
    );
    // The whole point is the trip factor: at least `2 * ITERS`, because every
    // remaining iteration retires the block's two decoded instructions.
    assert!(
        stats.exec.jit_insns >= 2 * ITERS,
        "expected at least {} retired instructions, got {}",
        2 * ITERS,
        stats.exec.jit_insns
    );
    // ...and no runaway over-count: the loop retires exactly `2 * ITERS`
    // instructions, so anything near a multiple of that means the counter is
    // being bumped somewhere it should not be.
    assert!(
        stats.exec.jit_insns <= 2 * ITERS + 2 * ITERS / 8,
        "trip counter over-counted: {} for {} iterations",
        stats.exec.jit_insns,
        ITERS
    );
    // The budget carrier and the stats counter are the same number: the engine
    // loop's `executed` accounting must see real work, not block entries.
    assert_eq!(
        u64::try_from(retired_total).unwrap_or(u64::MAX),
        stats.exec.jit_insns.saturating_add(stats.exec.iced_insns),
        "step_one's retired count must reconstruct the stats counters"
    );
    // Shared boot-gating counter sees the same dynamic volume.
    assert!(
        cpu.shared.guest_insns.load(Ordering::Relaxed) >= 2 * ITERS,
        "shared guest_insns must observe the dynamic count, got {}",
        cpu.shared.guest_insns.load(Ordering::Relaxed)
    );
}

/// A block that does not loop is charged exactly its static length: the trip
/// counter is seeded, never bumped. Guards the other direction — the fix for
/// the self-loop undercount must not inflate straight-line coverage.
#[test]
fn straight_line_block_is_charged_its_static_length() {
    let mut cpu = JitCpu::open_x86_64();
    cpu.virtual_alloc(
        TRIP_BASE + 0x2000,
        0x1000,
        MEM_RESERVE | MEM_COMMIT,
        protect::PAGE_EXECUTE_READWRITE,
    )
    .expect("alloc");
    let base = TRIP_BASE + 0x2000;
    // Three lowerable ALU insns and nothing else. No terminator, so the block
    // ends where the following `ud2` begins — `ud2` is not lowerable, so it is
    // excluded from the block and never reached by this test.
    let code = [
        0xb8, 0x01, 0x00, 0x00, 0x00, // mov eax, 1
        0x83, 0xc0, 0x02, // add eax, 2
        0x83, 0xc0, 0x04, // add eax, 4
        0x0f, 0x0b, // ud2 (decode stopper)
    ];
    cpu.mem_write(base, &code).expect("write block");
    cpu.write_rip(base).expect("rip");

    // One native frame: `cfg(test)` hotness is 0, so the block compiles on its
    // first visit and all three instructions retire together.
    let (result, retired) = cpu.step_one().expect("step");
    assert!(
        matches!(result, StepResult::Continue),
        "the block must run to its fallthrough, got {result:?}"
    );
    assert_eq!(cpu.thread.regs.rax(), 7, "mov/add/add must all retire");
    assert_eq!(cpu.thread.regs.rip, base + 11, "block ends at the ud2");
    let stats = cpu.stats();
    assert_eq!(stats.exec.iced_insns, 0, "nothing may stay interpreted");
    assert_eq!(
        stats.exec.jit_insns, 3,
        "a straight-line block runs each of its 3 instructions exactly once"
    );
    assert_eq!(
        u64::try_from(retired).unwrap_or(u64::MAX),
        3,
        "the budget carrier must report the static length"
    );
}

/// The sharp form of the same contract: a self-loop with a **known** trip
/// count must be charged `trips * insn_count`, exactly.
///
/// `cfg(test)` hotness is 0, so the block compiles on its first visit and the
/// whole loop runs inside one native frame — the count is therefore a precise
/// equality, not a bound. This is what pins "one charge per trip" rather than
/// "one charge per block entry" (which would report 2) or "one charge per
/// instruction" (which would report 6 only by accident).
#[test]
fn short_self_loop_charges_exact_trip_count() {
    const ITERS: u64 = 3;
    let mut cpu = JitCpu::open_x86_64();
    cpu.virtual_alloc(
        TRIP_BASE + 0x4000,
        0x1000,
        MEM_RESERVE | MEM_COMMIT,
        protect::PAGE_EXECUTE_READWRITE,
    )
    .expect("alloc");
    let base = TRIP_BASE + 0x4000;
    cpu.mem_write(base, &[0xff, 0xc9, 0x75, 0xfc])
        .expect("write loop");
    cpu.thread.regs.set_gpr(1, ITERS);
    cpu.write_rip(base).expect("rip");

    let (result, retired) = cpu.step_one().expect("step");
    assert!(
        matches!(result, StepResult::Continue),
        "one native frame must run the whole loop, got {result:?}"
    );
    assert_eq!(
        cpu.thread.regs.rip,
        base + 4,
        "the loop must exit past its terminator"
    );
    assert_eq!(
        cpu.thread.regs.gpr(1),
        0,
        "the loop must count down to zero"
    );
    let stats = cpu.stats();
    assert_eq!(
        stats.exec.iced_insns, 0,
        "the loop must never be interpreted"
    );
    assert_eq!(
        stats.exec.jit_insns,
        2 * ITERS,
        "3 trips of a 2-instruction block is 6 retired instructions"
    );
    assert_eq!(
        u64::try_from(retired).unwrap_or(u64::MAX),
        2 * ITERS,
        "the budget carrier must report the same dynamic count"
    );
}
