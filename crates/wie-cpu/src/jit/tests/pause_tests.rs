// `PAUSE` (F3 90) coverage: decoder-level no-op contract + JIT lowering.
//
// Split out of `tests/mod.rs` (file-size policy, ADR-002). Uses the shared
// dual-engine helper from the parent module.
use super::{SIMD_BASE, assert_same_regs, simd_dual};
use iced_x86::{Code, Decoder, DecoderOptions, InstructionInfoFactory};

/// `PAUSE` must be architecturally inert, otherwise the JIT no-op lowering
/// (see `jit::lower::insn`'s true-no-op arm) would be unsound. This pins the
/// contract at the decoder level so an iced upgrade that starts giving
/// `PAUSE` an operand or a flag effect fails here rather than silently
/// desynchronising the two engines.
#[test]
fn pause_is_architecturally_a_noop() {
    // F3 90 decodes to the dedicated `Pause` code, not a rep-prefixed `Nop`;
    // that dedicated code is what both the lowerer and the interpreter match.
    let bytes = [0xF3_u8, 0x90_u8];
    let mut dec = Decoder::with_ip(64, &bytes, SIMD_BASE, DecoderOptions::NONE);
    let insn = dec.decode();
    assert_eq!(insn.code(), Code::Pause, "F3 90 must decode as Pause");
    assert_eq!(
        insn.len(),
        2,
        "PAUSE is a fixed 2-byte encoding with no operand and no modrm"
    );
    assert_eq!(
        insn.op_count(),
        0,
        "PAUSE has no operands, so it cannot read or write a register or memory"
    );
    assert!(
        !insn.is_ip_rel_memory_operand() && !insn.is_jcc_short_or_near(),
        "PAUSE is neither a memory reference nor a branch"
    );

    // No architectural state whatsoever: no registers read or written, no
    // RFLAGS bits read, written, set, cleared or left undefined. That is what
    // makes it a true no-op here.
    assert_eq!(
        insn.rflags_read(),
        0,
        "PAUSE is a hint: it reads no RFLAGS bits"
    );
    assert_eq!(
        insn.rflags_written(),
        0,
        "PAUSE is a hint: it writes no RFLAGS bits"
    );
    assert_eq!(
        insn.rflags_modified(),
        0,
        "PAUSE is a hint: it modifies no RFLAGS bits at all"
    );
    let mut factory = InstructionInfoFactory::new();
    let info = factory.info(&insn);
    assert!(
        info.used_registers().is_empty(),
        "PAUSE must not use registers (got {:?})",
        info.used_registers()
    );
    assert!(
        info.used_memory().is_empty(),
        "PAUSE must not access memory"
    );
}

/// A basic block containing `PAUSE` must compile and execute entirely on the
/// JIT. `simd_dual` asserts `iced_insns == 0` plus register/RFLAGS equality
/// with the interpreter over the same bytes; without the lowerer arm the block
/// degrades to iced and the helper fails.
#[test]
fn pause_lowers_and_runs_on_the_jit() {
    // Three PAUSEs between flag-neutral nops: the block must compile, and both
    // engines must land RIP on the appended `ud2` terminator.
    let (iced_regs, jit_regs) = simd_dual(&[0xF3, 0x90, 0x90, 0xF3, 0x90, 0xF3, 0x90], &[], |_| {});
    assert_same_regs(&iced_regs, &jit_regs, "pause");
    // 7 encoded bytes + the helper's `nop` filler = 8, then `ud2` is decoded
    // but never retired (it stops the block).
    assert_eq!(
        jit_regs.rip,
        SIMD_BASE + 8,
        "every PAUSE and the nop filler must retire before the ud2 terminator"
    );
}
