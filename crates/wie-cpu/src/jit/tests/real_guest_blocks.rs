//! Real-guest blocks that the lowerer must accept, pinned byte-for-byte.
//!
//! Each fixture is copied out of a guest binary this project actually runs, with
//! the addresses preserved: RIP-relative operands are resolved against the same
//! offsets the image has, so the fixture exercises the operand forms real compiler
//! output produces rather than a hand-written approximation of them. A refusal here
//! is not a test-harness problem — it is a block that silently drops to the iced
//! interpreter for the rest of the run, which is what
//! `jit block lowering refused — block falls back to the interpreter` in a guest
//! log means.

use super::harness::alloc_code_region;
use super::*;
use crate::IcedCpu;

/// `real_exes/2048.exe` @ `0x140001b96` — the SSE2 scoring loop in `gameEnded`.
///
/// ```text
/// movd   xmm0, [rdx]          ; read one of four bytes
/// movd   xmm3, [rip+0x38d6]   ; the 0x000000ff-per-dword mask
/// add    rdx, 4
/// pcmpeqb xmm0, xmm2
/// pand   xmm0, xmm3
/// movd   eax, xmm0
/// movd   xmm0, eax
/// psadbw xmm0, xmm1
/// movd   eax, xmm0
/// add    r8d, eax
/// cmp    rdx, r11
/// jne    0x140001b96
/// ```
///
/// The loop was refused as `kind="unsupported-instruction"` twice in one 2048
/// session (once per call site), so every `gameEnded` call re-interpreted 48 bytes
/// of SSE2 it should have compiled. Note it uses `movd` in four distinct forms
/// (xmm←r32, r32←xmm, xmm←m32, and a RIP-relative m32) while the admission corpus
/// next to the gate only covered the two register forms — which is exactly the gap
/// a real image finds and a synthetic case does not.
const GAME_ENDED_LOOP: [u8; 48] = [
    0x66, 0x0F, 0x6E, 0x02, // movd xmm0, [rdx]
    0x66, 0x0F, 0x6E, 0x1D, 0xD6, 0x38, 0x00, 0x00, // movd xmm3, [rip+0x38d6]
    0x48, 0x83, 0xC2, 0x04, // add rdx, 4
    0x66, 0x0F, 0x74, 0xC2, // pcmpeqb xmm0, xmm2
    0x66, 0x0F, 0xDB, 0xC3, // pand xmm0, xmm3
    0x66, 0x0F, 0x7E, 0xC0, // movd eax, xmm0
    0x66, 0x0F, 0x6E, 0xC0, // movd xmm0, eax
    0x66, 0x0F, 0xF6, 0xC1, // psadbw xmm0, xmm1
    0x66, 0x0F, 0x7E, 0xC0, // movd eax, xmm0
    0x41, 0x01, 0xC0, // add r8d, eax
    0x4C, 0x39, 0xDA, // cmp rdx, r11
    0x75, 0xD0, // jne +0xd0 (self)
];

/// The fixture is **relocated**, not rewritten: the displacement is unchanged, so
/// the RIP-relative operand still resolves `0x38e2` ahead of the code — the same
/// distance as `0x140005478 - 0x140001b96` in the image — and the instruction
/// bytes stay byte-identical. The image's own base is unavailable because the
/// engine already reserves `0x140000000` for the guest module, so each run takes a
/// fresh slot instead (`alloc_region`).
const IMAGE_SIZE: usize = 0x8000;
/// `0x140005478 - 0x140001b96`, i.e. `next_ip + 0x38d6`.
const RIP_DATA_OFFSET: u64 = 0x38e2;
const SCRATCH_SIZE: usize = 0x1000;

/// Map the code slot and the scratch slot and lay the fixture down; returns
/// `(code, scratch)`.
fn lay_out(cpu: &mut dyn CpuEngine, code_byte_six: u8) -> (u64, u64) {
    let code = alloc_code_region(cpu, IMAGE_SIZE);
    let scratch = alloc_code_region(cpu, SCRATCH_SIZE);
    cpu.mem_write(code, &GAME_ENDED_LOOP)
        .expect("write the fixture");
    // The mask constant the loop loads RIP-relatively: 0x000000ff per dword, the
    // value 2048's compiler emits for a per-byte `pcmpeqb` result.
    cpu.mem_write(code + RIP_DATA_OFFSET, &[code_byte_six; 16])
        .expect("write the mask");
    cpu.mem_write(scratch, &[1, 0, 2, 0, 3, 0, 4, 0])
        .expect("write scratch");
    (code, scratch)
}

/// Two iterations, then `cmp rdx, r11` matches and the loop falls through.
fn enter(cpu: &mut dyn CpuEngine, code: u64, scratch: u64) -> u64 {
    let mut ctx = cpu.snapshot_thread_context();
    ctx.gpr[2] = scratch; // RDX
    ctx.gpr[11] = scratch + 8; // R11
    ctx.rip = code;
    cpu.restore_thread_context(&ctx);
    code + GAME_ENDED_LOOP.len() as u64
}

/// The block compiles. If this fails the guest log said `unsupported-instruction`
/// and every run of that loop paid the interpreter instead.
#[test]
fn the_2048_gameended_loop_compiles() {
    let mut cpu = Box::new(JitCpu::open_x86_64());
    let (code, scratch) = lay_out(cpu.as_mut(), 0xFF);
    enter(cpu.as_mut(), code, scratch);

    let compiled = cpu
        .try_compile(code)
        .expect("2048's gameEnded loop must lower as one block");
    assert_eq!(
        compiled.insn_count, 12,
        "the block is the twelve instructions above"
    );
}

/// And it computes what the interpreter computes: the whole register file and the
/// untouched scratch bytes agree after both engines run the same two iterations.
///
/// The JIT verdict above says "no refusal"; this says the emitter for the four
/// `movd` forms is *right*, which a compile-only assertion cannot.
#[test]
fn the_2048_gameended_loop_matches_the_interpreter() {
    let run = |jit: bool| {
        let mut buf = [0_u8; 8];
        buf.copy_from_slice(&[1, 0, 2, 0, 3, 0, 4, 0]);
        if jit {
            let mut cpu = Box::new(JitCpu::open_x86_64());
            let (code, scratch) = lay_out(cpu.as_mut(), 0xFF);
            let exit = enter(cpu.as_mut(), code, scratch);
            let compiled = cpu.try_compile(code).expect("compile");
            cpu.insert_ready(code, compiled);
            // One step runs the whole block; it leaves at the fallthrough.
            let _ = cpu.step_one().expect("jit step");
            assert_eq!(cpu.snapshot_thread_context().rip, exit, "jit exit rip");
            (cpu.snapshot_thread_context(), code)
        } else {
            let mut cpu = IcedCpu::open_x86_64();
            let (code, scratch) = lay_out(&mut cpu, 0xFF);
            let exit = enter(&mut cpu, code, scratch);
            let mut steps = 0;
            while cpu.snapshot_thread_context().rip != exit {
                cpu.step_once().expect("iced step");
                steps += 1;
                assert!(steps < 200, "the iced loop must terminate");
            }
            (cpu.snapshot_thread_context(), code)
        }
    };

    let (jit, jit_code) = run(true);
    let (iced, iced_code) = run(false);
    // RDX and R11 hold the fixture's own addresses — the loop walks RDX to the
    // R11 limit — and each run takes its own fresh slot, so only their *offsets*
    // are comparable. Everything else is bit-identical or the block is wrong.
    for i in 0..16 {
        if i == 2 || i == 11 {
            continue;
        }
        assert_eq!(jit.gpr[i], iced.gpr[i], "GPR{i} after the loop");
    }
    assert_eq!(
        jit.gpr[2].wrapping_sub(jit.gpr[11]),
        iced.gpr[2].wrapping_sub(iced.gpr[11]),
        "the loop advanced RDX by the same amount on both engines"
    );
    assert_eq!(
        jit.rip - jit_code,
        iced.rip - iced_code,
        "exit RIP, relative to the block, after the loop"
    );
    assert_eq!(
        jit.xmm[0], iced.xmm[0],
        "XMM0 carries the psadbw result out of the loop"
    );
}
