use super::chain_tests::chain_dual;
use super::*;
use crate::CpuEngine;
use crate::regs::RegFile;

/// ── high-byte TEST differential (Doom Retro strlen tail) ────
///
/// The DEHACKED strlen walks bytes of the last loaded qword using high-byte
/// register tests with NO REX prefix:
///
/// ```text
/// 84 d2    testb %dl, %dl     ; DL = RDX bits 0..7
/// 84 f6    testb %dh, %dh     ; DH = RDX bits 8..15
/// ```
///
/// If the JIT mis-executes the REX-less high-byte form (`84 f6`), the NUL
/// position is mis-detected and strlen overshoots — which corrupts Doom
/// Retro's DEHACKED line stripping (`lfstrip`) and breaks section matching.
#[test]
fn testb_high_byte_differential() {
    // mov edx, IMM32 ; test dh, dh ; jne L2 ;
    // L1: mov eax, 0x11111111 ; jmp END ;
    // L2: mov eax, 0x22222222 ; END: ud2
    let code_for = |imm: u32| -> Vec<u8> {
        let mut c = vec![0xBA]; // mov edx, imm32
        c.extend_from_slice(&imm.to_le_bytes());
        c.extend_from_slice(&[0x84, 0xF6]); // test dh, dh
        c.extend_from_slice(&[0x75, 0x07]); // jne L2 (+7)
        // L1: mov eax, 0x11111111
        c.extend_from_slice(&[0xB8, 0x11, 0x11, 0x11, 0x11]);
        c.extend_from_slice(&[0xEB, 0x05]); // jmp END (+5)
        // L2: mov eax, 0x22222222
        c.extend_from_slice(&[0xB8, 0x22, 0x22, 0x22, 0x22]);
        c.extend_from_slice(&[0x0F, 0x0B]); // ud2
        c
    };

    // DH = byte 1 of EDX.
    for (label, imm, want) in [
        ("DH nonzero", 0x0000_AB00_u32, 0x22222222_u64),
        ("DH zero", 0x0000_00CD_u32, 0x11111111_u64),
    ] {
        let stop_rip = SIMD_BASE + 25_u64;
        let setup = |regs: &mut RegFile| {
            regs.set_gpr(2, u64::from(imm));
        };
        let (iced, jit) = chain_dual(&code_for(imm), stop_rip, setup, |_: &mut dyn CpuEngine| {});
        assert_eq!(
            iced.gpr(0),
            want,
            "{label}: ICED took wrong branch (rax={:#x})",
            iced.gpr(0)
        );
        assert_eq!(
            jit.gpr(0),
            want,
            "{label}: JIT took wrong branch (rax={:#x})",
            jit.gpr(0)
        );
    }
}
