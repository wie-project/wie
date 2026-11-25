// Which guest registers a **self-loop** carries as header params, and what
// leaving them out must not change.
//
// A self-loop block's header used to carry all 16 GPRs plus rflags regardless of
// what the body touched (`live_eff.fill(true)`), and the shared exit block
// stored all 16 back. That is a register-file round trip per entry and per exit
// for values nobody reads — measured at 33/39/40 of 54/52/49
// parameter-materialisation instructions on the three SDL2 blocks (see
// `docs/architecture/prologue-epilogue-census.md`). The header now carries the
// live-in set, which is what these tests pin:
//
// 1. a register the loop only **writes** is still visible afterwards (the exit
//    store must happen even though the value was never a parameter —
//    `exit_gpr_loaded` is a superset of "defined" because every writer is also a
//    mention),
// 2. a register the loop never mentions is **preserved**: it is not loaded, so
//    the exit must not store the `iconst 0` placeholder over the guest's value
//    (this is the 7za corruption shape: `i` → null base → VA 0x1000),
// 3. the registers the loop does read still see the loop-carried value on the
//    second and later iterations, not the stale `JitCtx` word — the reason the
//    live set is enough for a loop-carried phi.
//
// Every case runs the same bytes on the iced interpreter and through a compiled
// block and compares the whole register file, including RFLAGS and RIP, so a
// wrong answer cannot hide in a register the test does not name.

use super::harness::map_code_region;
use super::simd_parity_tests::assert_same_regs;
use super::*;
use crate::IcedCpu;
use crate::exec::StepResult;
use crate::regs::RegFile;

/// Guest code region for this file. Fresh per test through [`map_code_region`]
/// at a distinct offset, so no two cases share an address (the harness'
/// distinct-VA rule).
const PARAM_BASE: u64 = 0x10a0_0000;

/// A self-loop whose body is `body` followed by `jne back-to-start`.
///
/// The loop exits by falling through the terminator, so the block is exactly
/// `body + jne` and `jne`'s target is the block's own start — the shape
/// `lower::emit::lower_self_loop_term` special-cases.
fn self_loop(body: &[u8]) -> Vec<u8> {
    let mut code = Vec::with_capacity(body.len() + 2);
    code.extend_from_slice(body);
    // `jne rel8` — 2 bytes, target = start of `body`.
    let rel = -((body.len() + 2) as i8);
    code.extend_from_slice(&[0x75, rel as u8]);
    code
}

/// Run `code` on both engines from `setup`'s state until RIP leaves the loop,
/// then return the two register files.
///
/// The code region holds `code` plus a `ud2` trap right after it, so a JIT that
/// mis-terminates faults instead of running off the page.
fn dual_loop(slot: u64, code: &[u8], setup: impl Fn(&mut RegFile)) -> (RegFile, RegFile) {
    let mut full = code.to_vec();
    full.push(0x0f);
    full.push(0x0b); // ud2
    let base = PARAM_BASE + slot * 0x1_0000;
    let exit_rip = base + code.len() as u64;
    let page = 0x2000_usize;

    // --- iced reference ---
    // The decode cache is thread-local and keyed by (rip, mem_gen); the fresh
    // CPUs below all share mem_gen 0, so flush before handing bytes to a VA.
    crate::exec::iced_decode_cache_flush();
    let mut iced = IcedCpu::open_x86_64();
    map_code_region(&mut iced, base, page);
    iced.mem_write(base, &full).expect("iced code");
    setup(iced.regs_mut());
    iced.write_rip(base).expect("iced rip");
    let mut steps = 0_usize;
    while iced.snapshot_thread_context().rip < exit_rip {
        iced.step_once().expect("iced step");
        steps = steps.saturating_add(1);
        assert!(steps < 10_000, "iced loop must terminate ({steps} steps)");
    }

    // --- JIT (blocks compile eagerly under cfg(test)) ---
    let mut cpu = JitCpu::open_x86_64();
    map_code_region(&mut cpu, base, page);
    cpu.mem_write(base, &full).expect("jit code");
    setup(&mut cpu.thread.regs);
    cpu.write_rip(base).expect("jit rip");
    let mut steps = 0_usize;
    loop {
        let (result, _retired) = cpu.step_one().expect("jit step");
        assert!(
            matches!(result, StepResult::Continue),
            "jit result {result:?} at rip {:#x}",
            cpu.thread.regs.rip
        );
        steps = steps.saturating_add(1);
        if cpu.thread.regs.rip >= exit_rip {
            break;
        }
        assert!(steps < 10_000, "jit loop must terminate ({steps} steps)");
        // Let the background compiler install the block if it is still pending.
        if steps == 1 && !cpu.has_ready_at(base) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !cpu.has_ready_at(base) && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            assert!(cpu.has_ready_at(base), "block must compile, not run iced");
        }
    }

    (iced.regs().clone(), cpu.thread.regs.clone())
}

/// `inc r12d` (`41 FF C4`) — writes a callee-saved register the loop never reads.
const INC_R12D: [u8; 3] = [0x41, 0xFF, 0xC4];
/// `add r8d, ecx` (`41 01 C8`) — reads and writes r8, reads ecx.
const ADD_R8D_ECX: [u8; 3] = [0x41, 0x01, 0xC8];
/// `dec ecx` (`FF C9`) — reads and writes ecx.
const DEC_ECX: [u8; 2] = [0xFF, 0xC9];

fn loop_body() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&INC_R12D);
    body.extend_from_slice(&ADD_R8D_ECX);
    body.extend_from_slice(&DEC_ECX);
    body
}

/// The register the loop writes but never reads must survive the loop, and the
/// one it never mentions must not be clobbered by a placeholder store.
#[test]
fn self_loop_write_only_and_untouched_registers_survive() {
    const SLOT: u64 = 0;
    let code = self_loop(&loop_body());
    const R12_INIT: u64 = 0xdead_beef;
    const R15_INIT: u64 = 0x0000_5555_aaaa_7777;
    const R8_INIT: u64 = 0x1000;
    let (iced, jit) = dual_loop(SLOT, &code, |r: &mut RegFile| {
        r.set_gpr(1, 5); // rcx: the loop counter, read and written
        r.set_gpr(8, R8_INIT); // r8: accumulated
        r.set_gpr(12, R12_INIT); // r12: written, never read
        r.set_gpr(15, R15_INIT); // r15: never mentioned at all
    });
    assert_eq!(jit.gpr(12), R12_INIT + 5, "r12 counts the iterations");
    assert_eq!(jit.gpr(15), R15_INIT, "r15 is untouched by the loop");
    assert_eq!(jit.gpr(8), R8_INIT + 15, "r8 accumulates 5+4+3+2+1");
    assert_same_regs(&iced, &jit, "self-loop write-only/untouched");
}

/// The loop-carried read must see the *previous iteration's* write, not the
/// value in `JitCtx` at entry — the property that makes the live-in set
/// sufficient for a phi (a register written before it is read in the same pass
/// is not loop-carried; one read before it is written must be).
#[test]
fn self_loop_carries_a_value_read_before_it_is_written() {
    const SLOT: u64 = 1;
    // `add r8d, ecx` then `dec ecx`: r8 reads ecx before ecx is written, so ecx
    // must be a header param. If it were dropped, iterations 2..5 would read the
    // entry value (5) and r8 would be 5*5 instead of 15.
    let code = self_loop(&loop_body());
    let (iced, jit) = dual_loop(SLOT, &code, |r: &mut RegFile| {
        r.set_gpr(1, 5);
        r.set_gpr(8, 0);
        r.set_gpr(12, 0);
        r.set_gpr(15, 0);
    });
    assert_eq!(jit.gpr(1), 0, "the counter reaches zero");
    assert_eq!(jit.gpr(8), 15, "each iteration reads the live counter");
    assert_same_regs(&iced, &jit, "self-loop loop-carried read");
}

/// A register written *after* being read in the same pass is not loop-carried:
/// dropping it from the params must not change the answer either.
#[test]
fn self_loop_where_the_counter_is_written_before_it_is_read() {
    const SLOT: u64 = 2;
    // `dec edx` (FF CA) writes, then `add r8d, edx` reads it in the same pass;
    // `dec ecx` comes last so the `jne` consumes *its* ZF, not the add's. The
    // incoming edx is dead — every pass overwrites it before reading it — so a
    // header param that carried the entry value would give a different sum.
    let mut body = Vec::new();
    body.extend_from_slice(&[0xFF, 0xCA]);
    body.extend_from_slice(&[0x41, 0x01, 0xD0]);
    body.extend_from_slice(&DEC_ECX);
    let code = self_loop(&body);
    let (iced, jit) = dual_loop(SLOT, &code, |r: &mut RegFile| {
        r.set_gpr(1, 5); // rcx: the counter
        r.set_gpr(2, 5); // rdx: overwritten before it is ever read
        r.set_gpr(8, 0);
    });
    // 4+3+2+1+0 from rdx's own decrements. Reading the incoming edx on every pass
    // instead of this pass' write would give 5 x 5 = 25.
    assert_eq!(
        (iced.gpr(8), jit.gpr(8)),
        (10, 10),
        "the decremented value is what is read"
    );
    assert_eq!(jit.gpr(2), 0, "rdx counts down from 5 with the loop");
    assert_same_regs(&iced, &jit, "self-loop write-then-read");
}
