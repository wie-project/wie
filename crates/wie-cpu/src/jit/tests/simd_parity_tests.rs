use super::*;

// --- B4: integer-SIMD JIT family — iced vs JIT dual-path gates ---
//
// Every newly-lowered SSE2 mnemonic runs once on the iced interpreter
// (reference) and once through a compiled JIT block with identical guest
// state; the two register files must match exactly (GPRs, XMMs, RFLAGS,
// RIP). The `has_ready_at` assertion guarantees the block actually went
// through Cranelift rather than silently falling back to iced.

use crate::IcedCpu;
use crate::regs::Rflags;
use iced_x86::Register;

/// Run `code` (a single op; a trailing `nop; ud2` is appended to reach the
/// 2-insn compile minimum and to stop linear decode past our bytes — the
/// zero-filled tail would otherwise decode as `add [rax], al` and extend
/// the block, faulting on the unmapped address) on iced and the JIT with
/// identical guest state.
pub(super) fn simd_dual(
    code: &[u8],
    data: &[u8],
    setup: impl Fn(&mut RegFile),
) -> (RegFile, RegFile) {
    let mut full = Vec::with_capacity(code.len() + 3);
    full.extend_from_slice(code);
    full.extend_from_slice(&[0x90, 0x0f, 0x0b]); // nop filler + ud2 terminator
    let n_insns = decode_count(&full);

    // --- iced reference ---
    // The iced decode cache is thread-local and keyed by (rip, mem_gen);
    // fresh test CPUs all share mem_gen 0, so flush or a later case at the
    // same base would decode the previous case's instruction.
    crate::exec::iced_decode_cache_flush();
    let mut iced = IcedCpu::open_x86_64();
    map_code_region(&mut iced, SIMD_BASE, 0x2000);
    iced.mem_write(SIMD_BASE, &full).expect("iced code");
    iced.mem_write(SIMD_DATA, data).expect("iced data");
    setup(iced.regs_mut());
    iced.write_rip(SIMD_BASE).expect("iced rip");
    for _ in 0..n_insns {
        iced.step_once().expect("iced step");
    }

    // --- JIT (eager compile: hotness is 0 under cfg(test)) ---
    let mut cpu = JitCpu::open_x86_64();
    map_code_region(&mut cpu, SIMD_BASE, 0x2000);
    cpu.mem_write(SIMD_BASE, &full).expect("jit code");
    cpu.mem_write(SIMD_DATA, data).expect("jit data");
    setup(&mut cpu.thread.regs);
    cpu.write_rip(SIMD_BASE).expect("jit rip");
    let (result, _retired) = cpu.step_one().expect("jit step");
    assert!(
        matches!(result, StepResult::Continue),
        "jit result {result:?}"
    );
    // The bg compiler installs asynchronously; with the IR verifier
    // enabled (always-on) its first compile of a block costs a bit more.
    // Wait briefly for the install rather than racing it. A block that
    // genuinely cannot JIT falls back to interpretation immediately
    // (iced_insns > 0) and fails fast below.
    if cpu.stats().exec.iced_insns == 0 && !cpu.has_ready_at(SIMD_BASE) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !cpu.has_ready_at(SIMD_BASE)
            && cpu.stats().exec.iced_insns == 0
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    assert!(
        cpu.has_ready_at(SIMD_BASE),
        "block must compile, not run iced"
    );
    assert_eq!(
        cpu.stats().exec.iced_insns,
        0,
        "block ran on iced instead of JIT"
    );

    // Both engines ran the same bytes over the same planted data: any
    // divergence in the 16 data bytes is a store-side lowering bug (loads
    // leave them untouched, so this is vacuously true for load-form cases).
    if !data.is_empty() {
        let mut iced_mem = [0_u8; 16];
        let mut jit_mem = [0_u8; 16];
        iced.mem_read(SIMD_DATA, &mut iced_mem)
            .expect("iced mem readback");
        cpu.mem_read(SIMD_DATA, &mut jit_mem)
            .expect("jit mem readback");
        assert_eq!(iced_mem, jit_mem, "guest memory diverged at SIMD_DATA");
    }

    (iced.regs().clone(), cpu.thread.regs.clone())
}

pub(super) fn assert_same_regs(iced: &RegFile, jit: &RegFile, what: &str) {
    for i in 0..16 {
        assert_eq!(iced.gpr(i), jit.gpr(i), "{what}: gpr[{i}]");
        assert_eq!(iced.xmm_at(i), jit.xmm_at(i), "{what}: xmm[{i}]");
    }
    assert_eq!(iced.rflags, jit.rflags, "{what}: rflags");
    assert_eq!(iced.rip, jit.rip, "{what}: rip");
}

fn set_pair(regs: &mut RegFile, x0: u128, x1: u128) {
    regs.write_xmm(Register::XMM0, x0).expect("xmm0");
    regs.write_xmm(Register::XMM1, x1).expect("xmm1");
}

#[test]
fn simd_half_lane_moves_match_iced() {
    // Half-lane moves (one 64-bit XMM lane ↔ m64). ModRM 0x01 = [rcx] with
    // RCX = SIMD_DATA. Encodings:
    // - 66 0F 12/13 /r  MOVLPD load/store   - 0F 12/13 /r  MOVLPS load/store
    // - 66 0F 16/17 /r  MOVHPD load/store   - 0F 16/17 /r  MOVHPS load/store
    let old_lo: u64 = 0x0123_4567_89ab_cdef;
    let old_hi: u64 = 0xfeed_face_dead_beef;
    let old_xmm = (u128::from(old_hi) << 64) | u128::from(old_lo);
    let m64: u64 = 0x8000_0001_ffff_0002;
    let setup_load = |r: &mut RegFile| {
        set_pair(r, old_xmm, 0);
        r.set_gpr_public(1, SIMD_DATA);
    };

    // Loads: the addressed lane is replaced; the other lane is untouched.
    for (name, bytes, want) in [
        (
            "movlpd load",
            &[0x66, 0x0f, 0x12, 0x01][..],
            (u128::from(old_hi) << 64) | u128::from(m64),
        ),
        (
            "movlps load",
            &[0x0f, 0x12, 0x01][..],
            (u128::from(old_hi) << 64) | u128::from(m64),
        ),
        (
            "movhpd load",
            &[0x66, 0x0f, 0x16, 0x01][..],
            (u128::from(m64) << 64) | u128::from(old_lo),
        ),
        (
            "movhps load",
            &[0x0f, 0x16, 0x01][..],
            (u128::from(m64) << 64) | u128::from(old_lo),
        ),
    ] {
        let (iced, jit) = simd_dual(bytes, &m64.to_le_bytes(), setup_load);
        assert_same_regs(&iced, &jit, name);
        assert_eq!(iced.xmm_at(0), want, "iced {name}");
        assert_eq!(jit.xmm_at(0), want, "jit {name}");
    }

    // Stores: memory receives exactly one qword (checked by simd_dual's
    // cross-engine readback); the source register must stay intact.
    let zero_setup = |r: &mut RegFile| {
        set_pair(r, old_xmm, 0);
        r.set_gpr_public(1, SIMD_DATA);
    };
    for (name, bytes) in [
        ("movlpd store", &[0x66, 0x0f, 0x13, 0x01][..]),
        ("movlps store", &[0x0f, 0x13, 0x01][..]),
        ("movhpd store", &[0x66, 0x0f, 0x17, 0x01][..]),
        ("movhps store", &[0x0f, 0x17, 0x01][..]),
    ] {
        let (iced, jit) = simd_dual(bytes, &[0_u8; 8], zero_setup);
        assert_same_regs(&iced, &jit, name);
        assert_eq!(
            iced.xmm_at(0),
            old_xmm,
            "{name} must leave the source xmm intact"
        );
    }

    // Absolute store-content check: run one store case on a bare iced CPU
    // (the oracle verified above to match the JIT) and assert what landed
    // in memory byte-for-byte.
    let mut cpu = IcedCpu::open_x86_64();
    map_code_region(&mut cpu, SIMD_BASE, 0x2000);
    crate::exec::iced_decode_cache_flush();
    let full = {
        let mut v = vec![0x66, 0x0f, 0x17, 0x01];
        v.extend_from_slice(&[0x90, 0x0f, 0x0b]);
        v
    };
    cpu.mem_write(SIMD_BASE, &full).expect("code");
    cpu.mem_write(SIMD_DATA, &[0_u8; 8]).expect("data");
    set_pair(cpu.regs_mut(), old_xmm, 0);
    cpu.regs_mut().set_gpr_public(1, SIMD_DATA);
    cpu.write_rip(SIMD_BASE).expect("rip");
    cpu.step_once().expect("step");
    let mut stored = [0_u8; 8];
    cpu.mem_read(SIMD_DATA, &mut stored).expect("readback");
    assert_eq!(
        stored,
        old_hi.to_le_bytes(),
        "movhpd store absolute content"
    );
}

#[test]
fn simd_packed_arith_matches_iced() {
    // Carry / borrow / sign patterns across every lane width.
    let x0 = 0xF1E2_D3C4_B5A6_9788_7766_5544_3322_1100_u128;
    let x1 = 0x0102_0304_0506_0708_090A_0B0C_0D0E_0F10_u128;
    for (name, bytes) in [
        ("paddb", &[0x66, 0x0f, 0xfc, 0xc1][..]),
        ("paddw", &[0x66, 0x0f, 0xfd, 0xc1][..]),
        ("paddd", &[0x66, 0x0f, 0xfe, 0xc1][..]),
        ("paddq", &[0x66, 0x0f, 0xd4, 0xc1][..]),
        ("psubb", &[0x66, 0x0f, 0xf8, 0xc1][..]),
        ("psubw", &[0x66, 0x0f, 0xf9, 0xc1][..]),
        ("psubd", &[0x66, 0x0f, 0xfa, 0xc1][..]),
        ("psubq", &[0x66, 0x0f, 0xfb, 0xc1][..]),
    ] {
        let (iced, jit) = simd_dual(bytes, &[], |r| set_pair(r, x0, x1));
        assert_same_regs(&iced, &jit, name);
    }
    // Hand-check PADDD (lane-wise 32-bit adds).
    let a = 0x0000_0002_0000_0001_0000_0002_0000_0001_u128;
    let b = 0x0000_0004_0000_0003_0000_0004_0000_0003_u128;
    let (iced, jit) = simd_dual(&[0x66, 0x0f, 0xfe, 0xc1], &[], |r| set_pair(r, a, b));
    let want = 0x0000_0006_0000_0004_0000_0006_0000_0004_u128;
    assert_eq!(iced.xmm_at(0), want, "iced paddd");
    assert_eq!(jit.xmm_at(0), want, "jit paddd");
}

/// Reference `PMOVMSKB` mask for a 128-bit xmm value (byte i sign → bit i).
fn pmovmskb_expected(x: u128) -> u64 {
    let mut mask = 0_u64;
    for i in 0_u64..16 {
        if (x >> (i.saturating_mul(8))) & 0x80 != 0 {
            mask |= 1_u64 << i;
        }
    }
    mask
}

#[test]
fn simd_pmovmskb_matches_iced() {
    // PMOVMSKB eax, xmm1 (66 0F D7 C1): pack the 16 byte-sign bits of
    // xmm1 into the low 16 bits of eax; the upper GPR bits are zeroed.
    // Low 8 bytes 0x80 → bits 0-7 set; high 8 bytes 0x00 → bits 8-15 clear.
    let x1 = 0x0000_0000_0000_0000_8080_8080_8080_8080_u128;
    let (iced, jit) = simd_dual(&[0x66, 0x0f, 0xd7, 0xc1], &[], |r| set_pair(r, 0, x1));
    assert_same_regs(&iced, &jit, "pmovmskb");
    assert_eq!(iced.gpr(0), 0xFF, "iced pmovmskb mask");
    assert_eq!(jit.gpr(0), 0xFF, "jit pmovmskb mask");

    // Every byte 0xFF → all 16 sign bits set; RAX stays 32-bit zero-extended.
    let (iced, jit) = simd_dual(&[0x66, 0x0f, 0xd7, 0xc1], &[], |r| {
        set_pair(r, 0, u128::MAX)
    });
    assert_same_regs(&iced, &jit, "pmovmskb all-set");
    assert_eq!(iced.gpr(0), 0xFFFF, "iced pmovmskb all-set");
    assert_eq!(jit.gpr(0), 0xFFFF, "jit pmovmskb all-set");
}

#[test]
fn simd_vpmovmskb_matches_iced() {
    // VEX VPMOVMSKB eax, xmm1 (C5 F9 D7 C1) — same semantics, own mnemonic.
    let x1 = 0x807F_0080_FF00_0001_8000_7F7F_8080_0100_u128;
    let (iced, jit) = simd_dual(&[0xc5, 0xf9, 0xd7, 0xc1], &[], |r| set_pair(r, 0, x1));
    assert_same_regs(&iced, &jit, "vpmovmskb");
    let want = pmovmskb_expected(x1);
    assert_eq!(iced.gpr(0), want, "iced vpmovmskb");
    assert_eq!(jit.gpr(0), want, "jit vpmovmskb");
}

#[test]
fn gpr_bsr_bsf_matches_iced() {
    // BSR/BSF: the index of the most/least significant set bit;
    // src == 0 → ZF=1, dst written 0.
    // 32-bit form (0F BD C1 / 0F BC C1 — eax, ecx) and the REX.W 64-bit
    // form (48 0F BD C1 / 48 0F BC C1 — rax, rcx).
    for (name, bytes32, bytes64) in [
        (
            "bsr",
            &[0x0f, 0xbd, 0xc1][..],
            &[0x48, 0x0f, 0xbd, 0xc1][..],
        ),
        (
            "bsf",
            &[0x0f, 0xbc, 0xc1][..],
            &[0x48, 0x0f, 0xbc, 0xc1][..],
        ),
    ] {
        for (label, rcx) in [
            ("low", 0x1_u64),
            ("mid", 0x0400_0000_u64),
            ("high", 0x8000_0000_u64),
            ("zero", 0_u64),
        ] {
            let (iced, jit) = simd_dual(bytes32, &[], |r| {
                r.set_gpr(1, rcx);
            });
            assert_same_regs(&iced, &jit, name);
            // 63 − leading_zeros works for both widths: the 32-bit operand's
            // zero-extended top half is counted by clz and cancels.
            let want = if rcx == 0 {
                0
            } else if name == "bsr" {
                u64::from(63_u32).saturating_sub(u64::from(rcx.leading_zeros()))
            } else {
                u64::from(rcx.trailing_zeros())
            };
            assert_eq!(iced.gpr(0), want, "iced {name} 32-bit {label}");
            assert_eq!(jit.gpr(0), want, "jit {name} 32-bit {label}");
        }
        for (label, rcx) in [
            ("low", 0x1_u64),
            ("high", 0x8000_0000_0000_0000_u64),
            ("zero", 0_u64),
        ] {
            let (iced, jit) = simd_dual(bytes64, &[], |r| {
                r.set_gpr(1, rcx);
            });
            assert_same_regs(&iced, &jit, name);
            let want = if rcx == 0 {
                0
            } else if name == "bsr" {
                u64::from(63_u32).saturating_sub(u64::from(rcx.leading_zeros()))
            } else {
                u64::from(rcx.trailing_zeros())
            };
            assert_eq!(iced.gpr(0), want, "iced {name} 64-bit {label}");
            assert_eq!(jit.gpr(0), want, "jit {name} 64-bit {label}");
        }
    }
}

#[test]
fn simd_saturating_arith_matches_iced() {
    // Signed/unsigned saturation edges: 0x7F+1, 0x80+0x80, 0x00-1, 0xFFFF+1.
    let x0 = 0x0000_0000_0000_0000_7F7F_7F80_0100_807F_u128;
    let x1 = 0x0000_0000_0000_0000_0101_0101_FFFF_8080_u128;
    for (name, bytes) in [
        ("paddsb", &[0x66, 0x0f, 0xec, 0xc1][..]),
        ("paddsw", &[0x66, 0x0f, 0xed, 0xc1][..]),
        ("paddusb", &[0x66, 0x0f, 0xdc, 0xc1][..]),
        ("paddusw", &[0x66, 0x0f, 0xdd, 0xc1][..]),
        ("psubsb", &[0x66, 0x0f, 0xe8, 0xc1][..]),
        ("psubsw", &[0x66, 0x0f, 0xe9, 0xc1][..]),
        ("psubusb", &[0x66, 0x0f, 0xd8, 0xc1][..]),
        ("psubusw", &[0x66, 0x0f, 0xd9, 0xc1][..]),
    ] {
        let (iced, jit) = simd_dual(bytes, &[], |r| set_pair(r, x0, x1));
        assert_same_regs(&iced, &jit, name);
    }
    // Hand-check PADDSB low byte: 0x7F + 0x01 → 0x7F (signed sat).
    let a = 0x0000_0000_0000_0000_0000_0000_0000_007F_u128;
    let b = 0x0000_0000_0000_0000_0000_0000_0000_0001_u128;
    let (iced, jit) = simd_dual(&[0x66, 0x0f, 0xec, 0xc1], &[], |r| set_pair(r, a, b));
    assert_eq!(iced.xmm_at(0) & 0xff, 0x7f, "iced paddsb sat");
    assert_eq!(jit.xmm_at(0) & 0xff, 0x7f, "jit paddsb sat");
    // PADDUSB: 0xFF + 0x01 → 0xFF (unsigned sat).
    let a = 0x0000_0000_0000_0000_0000_0000_0000_00FF_u128;
    let (iced, jit) = simd_dual(&[0x66, 0x0f, 0xdc, 0xc1], &[], |r| set_pair(r, a, b));
    assert_eq!(iced.xmm_at(0) & 0xff, 0xff, "iced paddusb sat");
    assert_eq!(jit.xmm_at(0) & 0xff, 0xff, "jit paddusb sat");
}

#[test]
fn simd_multiply_matches_iced() {
    let x0 = 0x0001_0002_0003_0004_0005_0006_0007_0008_u128;
    let x1 = 0x8000_7FFF_0100_0200_1000_2000_4000_8000_u128;
    for (name, bytes) in [
        ("pmullw", &[0x66, 0x0f, 0xd5, 0xc1][..]),
        ("pmulhw", &[0x66, 0x0f, 0xe5, 0xc1][..]),
        ("pmulhuw", &[0x66, 0x0f, 0xe4, 0xc1][..]),
        ("pmuludq", &[0x66, 0x0f, 0xf4, 0xc1][..]),
        ("pmaddwd", &[0x66, 0x0f, 0xf5, 0xc1][..]),
    ] {
        let (iced, jit) = simd_dual(bytes, &[], |r| set_pair(r, x0, x1));
        assert_same_regs(&iced, &jit, name);
    }
    // Hand-check PMULUDQ: qword0 = dword0(a)*dword0(b), qword1 = dword2.
    let a = 0x0000_0000_0000_0002_0000_0000_0000_0003_u128;
    let b = 0x0000_0000_0000_0004_0000_0000_0000_0005_u128;
    let (iced, jit) = simd_dual(&[0x66, 0x0f, 0xf4, 0xc1], &[], |r| set_pair(r, a, b));
    let want = 0x0000_0000_0000_0008_0000_0000_0000_000F_u128;
    assert_eq!(iced.xmm_at(0), want, "iced pmuludq");
    assert_eq!(jit.xmm_at(0), want, "jit pmuludq");
}

#[test]
fn simd_compare_matches_iced() {
    let x0 = 0x8180_7F00_0100_FFFF_807F_0001_8000_7FFF_u128;
    let x1 = 0x7F7F_7F7F_0101_FFFF_8080_0000_7FFF_8000_u128;
    for (name, bytes) in [
        ("pcmpeqb", &[0x66, 0x0f, 0x74, 0xc1][..]),
        ("pcmpeqw", &[0x66, 0x0f, 0x75, 0xc1][..]),
        ("pcmpeqd", &[0x66, 0x0f, 0x76, 0xc1][..]),
        ("pcmpgtb", &[0x66, 0x0f, 0x64, 0xc1][..]),
        ("pcmpgtw", &[0x66, 0x0f, 0x65, 0xc1][..]),
        ("pcmpgtd", &[0x66, 0x0f, 0x66, 0xc1][..]),
    ] {
        let (iced, jit) = simd_dual(bytes, &[], |r| set_pair(r, x0, x1));
        assert_same_regs(&iced, &jit, name);
    }
    // Hand-check PCMPGTD: 0x8000_0000 > 0x7FFF_FFFF is false (signed).
    let a = 0x0000_0000_0000_0000_0000_0000_8000_0000_u128;
    let b = 0x0000_0000_0000_0000_0000_0000_7FFF_FFFF_u128;
    let (iced, jit) = simd_dual(&[0x66, 0x0f, 0x66, 0xc1], &[], |r| set_pair(r, a, b));
    assert_eq!(iced.xmm_at(0) & 0xffff_ffff, 0, "iced pcmpgtd signed");
    assert_eq!(jit.xmm_at(0) & 0xffff_ffff, 0, "jit pcmpgtd signed");
}

#[test]
fn simd_shifts_matches_iced() {
    let x0 = 0xF0E0_D0C0_B0A0_9080_7060_5040_3020_1000_u128;
    // imm forms: dst xmm0, imm8 = 5.
    for (name, bytes) in [
        ("psllw imm", &[0x66, 0x0f, 0x71, 0xf0, 0x05][..]),
        ("pslld imm", &[0x66, 0x0f, 0x72, 0xf0, 0x05][..]),
        ("psllq imm", &[0x66, 0x0f, 0x73, 0xf0, 0x05][..]),
        ("psrlw imm", &[0x66, 0x0f, 0x71, 0xd0, 0x05][..]),
        ("psrld imm", &[0x66, 0x0f, 0x72, 0xd0, 0x05][..]),
        ("psrlq imm", &[0x66, 0x0f, 0x73, 0xd0, 0x05][..]),
        ("psraw imm", &[0x66, 0x0f, 0x71, 0xe0, 0x05][..]),
        ("psrad imm", &[0x66, 0x0f, 0x72, 0xe0, 0x05][..]),
    ] {
        let (iced, jit) = simd_dual(bytes, &[], |r| set_pair(r, x0, 0));
        assert_same_regs(&iced, &jit, name);
    }
    // variable forms: count lanes in xmm1.
    let cnt = 0x0000_0000_0000_0000_0004_0000_0010_0010_u128;
    for (name, bytes) in [
        ("psllw var", &[0x66, 0x0f, 0xf1, 0xc1][..]),
        ("pslld var", &[0x66, 0x0f, 0xf2, 0xc1][..]),
        ("psllq var", &[0x66, 0x0f, 0xf3, 0xc1][..]),
        ("psrlw var", &[0x66, 0x0f, 0xd1, 0xc1][..]),
        ("psrld var", &[0x66, 0x0f, 0xd2, 0xc1][..]),
        ("psrlq var", &[0x66, 0x0f, 0xd3, 0xc1][..]),
        ("psraw var", &[0x66, 0x0f, 0xe1, 0xc1][..]),
        ("psrad var", &[0x66, 0x0f, 0xe2, 0xc1][..]),
    ] {
        let (iced, jit) = simd_dual(bytes, &[], |r| set_pair(r, x0, cnt));
        assert_same_regs(&iced, &jit, name);
    }
    // Hand-check PSRLD imm 4: 0x0000_0000_1000_0000 → 0x0000_0000_0100_0000.
    let a = 0x0000_0000_0000_0000_0000_0000_1000_0000_u128;
    let (iced, jit) = simd_dual(&[0x66, 0x0f, 0x72, 0xd0, 0x04], &[], |r| set_pair(r, a, 0));
    assert_eq!(iced.xmm_at(0) & 0xffff_ffff, 0x0100_0000, "iced psrld");
    assert_eq!(jit.xmm_at(0) & 0xffff_ffff, 0x0100_0000, "jit psrld");
    // Oversized count (32 ≥ 32) → 0 per x86.
    let big = 0x0020_0020_0020_0020_0020_0020_0020_0020_u128;
    let (iced, jit) = simd_dual(&[0x66, 0x0f, 0xf2, 0xc1], &[], |r| set_pair(r, x0, big));
    assert_eq!(iced.xmm_at(0), 0, "iced pslld big-count zeroes");
    assert_eq!(jit.xmm_at(0), 0, "jit pslld big-count zeroes");
}

#[test]
fn simd_byte_shifts_match_iced() {
    // Psrldq/Pslldq: whole-128-bit byte shifts (66 0F 73 /3 ib and /7 ib).
    let x0 = 0xF0E0_D0C0_B0A0_9080_7060_5040_3020_1000_u128;
    for (name, bytes) in [
        ("psrldq imm1", &[0x66, 0x0f, 0x73, 0xd8, 0x01][..]),
        ("psrldq imm8", &[0x66, 0x0f, 0x73, 0xd8, 0x08][..]),
        ("psrldq imm15", &[0x66, 0x0f, 0x73, 0xd8, 0x0f][..]),
        ("psrldq imm16", &[0x66, 0x0f, 0x73, 0xd8, 0x10][..]),
        ("pslldq imm1", &[0x66, 0x0f, 0x73, 0xf8, 0x01][..]),
        ("pslldq imm8", &[0x66, 0x0f, 0x73, 0xf8, 0x08][..]),
        ("pslldq imm15", &[0x66, 0x0f, 0x73, 0xf8, 0x0f][..]),
        ("pslldq imm16", &[0x66, 0x0f, 0x73, 0xf8, 0x10][..]),
    ] {
        let (iced, jit) = simd_dual(bytes, &[], |r| set_pair(r, x0, 0));
        assert_same_regs(&iced, &jit, name);
    }
    // Hand-check PSRLDQ imm1: byte 0 dropped, zero-filled on the left.
    let (iced, jit) = simd_dual(&[0x66, 0x0f, 0x73, 0xd8, 0x01], &[], |r| set_pair(r, x0, 0));
    assert_eq!(
        iced.xmm_at(0),
        0x00F0_E0D0_C0B0_A090_8070_6050_4030_2010,
        "iced psrldq 1"
    );
    assert_eq!(
        jit.xmm_at(0),
        0x00F0_E0D0_C0B0_A090_8070_6050_4030_2010,
        "jit psrldq 1"
    );
    // Hand-check PSRLDQ imm15: only the top byte survives (shifted to bit 0).
    let (iced, jit) = simd_dual(&[0x66, 0x0f, 0x73, 0xd8, 0x0f], &[], |r| set_pair(r, x0, 0));
    assert_eq!(iced.xmm_at(0), 0xF0, "iced psrldq 15");
    assert_eq!(jit.xmm_at(0), 0xF0, "jit psrldq 15");
    // Hand-check PSRLDQ imm16 → zero.
    let (iced, jit) = simd_dual(&[0x66, 0x0f, 0x73, 0xd8, 0x10], &[], |r| set_pair(r, x0, 0));
    assert_eq!(iced.xmm_at(0), 0, "iced psrldq 16 zeroes");
    assert_eq!(jit.xmm_at(0), 0, "jit psrldq 16 zeroes");
}

#[test]
fn simd_cvtdq2pd_matches_iced() {
    // CVTDQ2PD xmm, xmm/m64: two packed signed dwords → two doubles.
    for (name, bytes, is_mem) in [
        ("cvtdq2pd reg", &[0xf3, 0x0f, 0xe6, 0xc1][..], false),
        ("cvtdq2pd mem", &[0xf3, 0x0f, 0xe6, 0x01][..], true),
    ] {
        let (iced, jit) = simd_dual(
            bytes,
            &[0x02, 0x00, 0x00, 0x00, 0xfe, 0xff, 0xff, 0xff],
            |r| {
                set_pair(r, 0, 0);
                if is_mem {
                    r.set_gpr_public(1, SIMD_DATA); // RCX = m64 base
                }
            },
        );
        assert_same_regs(&iced, &jit, name);
    }
    // Hand-check: dwords 1 and -2 → doubles 1.0 (low) and -2.0 (high).
    let src = 0xffff_fffe_0000_0001_u128;
    let expect = u128::from(1.0_f64.to_bits()) | (u128::from((-2.0_f64).to_bits()) << 64);
    let (iced, jit) = simd_dual(&[0xf3, 0x0f, 0xe6, 0xc1], &[], |r| set_pair(r, 0, src));
    assert_eq!(iced.xmm_at(0), expect, "iced cvtdq2pd 1,-2");
    assert_eq!(jit.xmm_at(0), expect, "jit cvtdq2pd 1,-2");
}

#[test]
fn simd_cvtps2pd_matches_iced() {
    // CVTPS2PD xmm, xmm/m64: two packed singles → two doubles (0F 5A).
    for (name, bytes, is_mem) in [
        ("cvtps2pd reg", &[0x0f, 0x5a, 0xc1][..], false),
        ("cvtps2pd mem", &[0x0f, 0x5a, 0x01][..], true),
    ] {
        let (iced, jit) = simd_dual(
            bytes,
            &[0x00, 0x00, 0x80, 0x3f, 0x00, 0x00, 0x00, 0xc0],
            |r| {
                set_pair(r, 0, 0);
                if is_mem {
                    r.set_gpr_public(1, SIMD_DATA); // RCX = m64 base
                }
            },
        );
        assert_same_regs(&iced, &jit, name);
    }
    // Hand-check: singles 1.0f and -2.0f → doubles 1.0 (low) and -2.0 (high).
    let src = u128::from(0xc000_0000_3f80_0000_u64);
    let expect = u128::from(1.0_f64.to_bits()) | (u128::from((-2.0_f64).to_bits()) << 64);
    let (iced, jit) = simd_dual(&[0x0f, 0x5a, 0xc1], &[], |r| set_pair(r, 0, src));
    assert_eq!(iced.xmm_at(0), expect, "iced cvtps2pd 1,-2");
    assert_eq!(jit.xmm_at(0), expect, "jit cvtps2pd 1,-2");
}

#[test]
fn simd_cvtpd2dq_matches_iced() {
    // CVTPD2DQ xmm, xmm/m128 (F2 0F E6): two doubles → two dwords, upper zeroed.
    for (name, bytes, is_mem) in [
        ("cvtpd2dq reg", &[0xf2, 0x0f, 0xe6, 0xc1][..], false),
        ("cvtpd2dq mem", &[0xf2, 0x0f, 0xe6, 0x01][..], true),
    ] {
        let (iced, jit) = simd_dual(
            bytes,
            &[
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf0, 0x3f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0xc0,
            ],
            |r| {
                set_pair(r, 0, 0x1234_5678_9abc_def0);
                if is_mem {
                    r.set_gpr_public(1, SIMD_DATA); // RCX = m128 base
                }
            },
        );
        assert_same_regs(&iced, &jit, name);
    }
    // Hand-check: doubles 1.0 and -2.0 → dwords 1 and -2 in the low 64 bits.
    let src = u128::from((-2.0_f64).to_bits()) << 64 | u128::from(1.0_f64.to_bits());
    let expect = u128::from(1_u64) | (u128::from(0xffff_fffe_u64) << 32);
    let (iced, jit) = simd_dual(&[0xf2, 0x0f, 0xe6, 0xc1], &[], |r| set_pair(r, 0, src));
    assert_eq!(iced.xmm_at(0), expect, "iced cvtpd2dq 1,-2");
    assert_eq!(jit.xmm_at(0), expect, "jit cvtpd2dq 1,-2");
}

#[test]
fn simd_cvtpd2ps_matches_iced() {
    // CVTPD2PS xmm, xmm/m128 (66 0F 5A): two doubles → two singles, upper zeroed.
    for (name, bytes, is_mem) in [
        ("cvtpd2ps reg", &[0x66, 0x0f, 0x5a, 0xc1][..], false),
        ("cvtpd2ps mem", &[0x66, 0x0f, 0x5a, 0x01][..], true),
    ] {
        let (iced, jit) = simd_dual(
            bytes,
            &[
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf0, 0x3f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0xc0,
            ],
            |r| {
                set_pair(r, 0, 0x1122_3344_5566_7788);
                if is_mem {
                    r.set_gpr_public(1, SIMD_DATA); // RCX = m128 base
                }
            },
        );
        assert_same_regs(&iced, &jit, name);
    }
    // Hand-check: doubles 1.0 and -2.0 → singles 1.0f and -2.0f.
    let src = u128::from((-2.0_f64).to_bits()) << 64 | u128::from(1.0_f64.to_bits());
    let expect = u128::from(1.0_f32.to_bits()) | (u128::from((-2.0_f32).to_bits()) << 32);
    let (iced, jit) = simd_dual(&[0x66, 0x0f, 0x5a, 0xc1], &[], |r| set_pair(r, 0, src));
    assert_eq!(iced.xmm_at(0), expect, "iced cvtpd2ps 1,-2");
    assert_eq!(jit.xmm_at(0), expect, "jit cvtpd2ps 1,-2");
}

#[test]
fn simd_cvtsd2ss_matches_iced() {
    // CVTSD2SS xmm, xmm/m64 (F2 0F 5A): low double → single, upper preserved.
    for (name, bytes, is_mem) in [
        ("cvtsd2ss reg", &[0xf2, 0x0f, 0x5a, 0xc1][..], false),
        ("cvtsd2ss mem", &[0xf2, 0x0f, 0x5a, 0x01][..], true),
    ] {
        let (iced, jit) = simd_dual(
            bytes,
            &[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf0, 0x3f],
            |r| {
                set_pair(r, 0, 0x1234_5678_9abc_def0_1122_3344_5566_7788);
                if is_mem {
                    r.set_gpr_public(1, SIMD_DATA); // RCX = m64 base
                }
            },
        );
        assert_same_regs(&iced, &jit, name);
    }
    // Hand-check: low double 1.0 → single 1.0f, bits 32-127 preserved.
    let old = u128::from(0xdead_beef_cafe_babe_u64) << 64 | u128::from(0x1122_3344_5566_7788_u64);
    let src = u128::from(1.0_f64.to_bits());
    let expect =
        (old & 0xffff_ffff_ffff_ffff_ffff_ffff_0000_0000_u128) | u128::from(1.0_f32.to_bits());
    let (iced, jit) = simd_dual(&[0xf2, 0x0f, 0x5a, 0xc1], &[], |r| set_pair(r, old, src));
    assert_eq!(iced.xmm_at(0), expect, "iced cvtsd2ss preserve");
    assert_eq!(jit.xmm_at(0), expect, "jit cvtsd2ss preserve");
}

#[test]
fn simd_cvtss2sd_matches_iced() {
    // CVTSS2SD xmm, xmm/m32 (F3 0F 5A): low single → double, upper preserved.
    for (name, bytes, is_mem) in [
        ("cvtss2sd reg", &[0xf3, 0x0f, 0x5a, 0xc1][..], false),
        ("cvtss2sd mem", &[0xf3, 0x0f, 0x5a, 0x01][..], true),
    ] {
        let (iced, jit) = simd_dual(
            bytes,
            &[0x00, 0x00, 0x80, 0x3f, 0x00, 0x00, 0x00, 0x00],
            |r| {
                set_pair(r, 0, 0x1234_5678_9abc_def0_1122_3344_5566_7788);
                if is_mem {
                    r.set_gpr_public(1, SIMD_DATA); // RCX = m32 base
                }
            },
        );
        assert_same_regs(&iced, &jit, name);
    }
    // Hand-check: low single 1.0f → double 1.0, bits 64-127 preserved.
    let old = u128::from(0xdead_beef_cafe_babe_u64) << 64 | u128::from(0x1122_3344_5566_7788_u64);
    let src = u128::from(1.0_f32.to_bits());
    let expect =
        (old & 0xffff_ffff_ffff_ffff_0000_0000_0000_0000_u128) | u128::from(1.0_f64.to_bits());
    let (iced, jit) = simd_dual(&[0xf3, 0x0f, 0x5a, 0xc1], &[], |r| set_pair(r, old, src));
    assert_eq!(iced.xmm_at(0), expect, "iced cvtss2sd preserve");
    assert_eq!(jit.xmm_at(0), expect, "jit cvtss2sd preserve");
}

#[test]
fn simd_pack_unpack_matches_iced() {
    let x0 = 0x807F_FF00_1234_5678_0001_FFFF_8000_7FFF_u128;
    let x1 = 0x0102_0304_0506_0708_090A_0B0C_0D0E_0F10_u128;
    for (name, bytes) in [
        ("packsswb", &[0x66, 0x0f, 0x63, 0xc1][..]),
        ("packssdw", &[0x66, 0x0f, 0x6b, 0xc1][..]),
        ("packuswb", &[0x66, 0x0f, 0x67, 0xc1][..]),
        ("punpcklbw", &[0x66, 0x0f, 0x60, 0xc1][..]),
        ("punpcklwd", &[0x66, 0x0f, 0x61, 0xc1][..]),
        ("punpckldq", &[0x66, 0x0f, 0x62, 0xc1][..]),
        ("punpcklqdq", &[0x66, 0x0f, 0x6c, 0xc1][..]),
        ("punpckhbw", &[0x66, 0x0f, 0x68, 0xc1][..]),
        ("punpckhwd", &[0x66, 0x0f, 0x69, 0xc1][..]),
        ("punpckhdq", &[0x66, 0x0f, 0x6a, 0xc1][..]),
        ("punpckhqdq", &[0x66, 0x0f, 0x6d, 0xc1][..]),
    ] {
        let (iced, jit) = simd_dual(bytes, &[], |r| set_pair(r, x0, x1));
        assert_same_regs(&iced, &jit, name);
    }
    // Hand-check PUNPCKLBW low bytes: [a0,b0,a1,b1,a2,b2,a3,b3].
    let a = 0x0000_0000_0000_0000_0000_0000_0000_0000_u128;
    let b = 0x0000_0000_0000_0000_0000_0000_0000_0000_u128;
    let a = a | 0x0000_0000_0000_0000_0000_0000_0403_0201_u128;
    let b = b | 0x0000_0000_0000_0000_0000_0000_0807_0605_u128;
    let (iced, jit) = simd_dual(&[0x66, 0x0f, 0x60, 0xc1], &[], |r| set_pair(r, a, b));
    let want = 0x0000_0000_0000_0000_0804_0703_0602_0501_u128;
    assert_eq!(iced.xmm_at(0), want, "iced punpcklbw");
    assert_eq!(jit.xmm_at(0), want, "jit punpcklbw");
    // Hand-check PACKSSWB: word 0x8000 saturates to byte 0x80.
    let a = 0x0000_0000_0000_0000_0000_0000_0000_8000_u128;
    let (iced, jit) = simd_dual(&[0x66, 0x0f, 0x63, 0xc1], &[], |r| set_pair(r, a, 0));
    assert_eq!(iced.xmm_at(0) & 0xff, 0x80, "iced packsswb");
    assert_eq!(jit.xmm_at(0) & 0xff, 0x80, "jit packsswb");
}

#[test]
fn simd_shuffles_match_iced() {
    let x0 = 0x1122_3344_5566_7788_99AA_BBCC_DDEE_FF00_u128;
    let x1 = 0x0011_2233_4455_6677_8899_AABB_CCDD_EEFF_u128;
    for (name, bytes) in [
        ("pshufd", &[0x66, 0x0f, 0x70, 0xc1, 0x1b][..]),
        ("pshuflw", &[0xf2, 0x0f, 0x70, 0xc1, 0x1b][..]),
        ("pshufhw", &[0xf3, 0x0f, 0x70, 0xc1, 0x1b][..]),
        ("shufpd", &[0x66, 0x0f, 0xc6, 0xc1, 0x01][..]),
    ] {
        let (iced, jit) = simd_dual(bytes, &[], |r| set_pair(r, x0, x1));
        assert_same_regs(&iced, &jit, name);
    }
    // Hand-check SHUFPD imm 0x01: low lane from src2, high lane from src1
    // (the UCRT wcscpy fast path uses this — the guest File menus died on
    // it before the iced implementation landed).
    let a = 0x1111_2222_3333_4444_5555_6666_7777_8888_u128;
    let b = 0xAAAA_BBBB_CCCC_DDDD_EEEE_FFFF_0000_1111_u128;
    let (iced, jit) = simd_dual(&[0x66, 0x0f, 0xc6, 0xc1, 0x01], &[], |r| set_pair(r, a, b));
    let want = 0x1111_2222_3333_4444_EEEE_FFFF_0000_1111_u128;
    assert_eq!(iced.xmm_at(0), want, "iced shufpd");
    assert_eq!(jit.xmm_at(0), want, "jit shufpd");
    // Hand-check PSHUFD imm 0x1B = [3,2,1,0] (reverse dwords).
    let a = 0x0000_0001_0000_0002_0000_0003_0000_0004_u128;
    let (iced, jit) = simd_dual(&[0x66, 0x0f, 0x70, 0xc1, 0x1b], &[], |r| set_pair(r, 0, a));
    let want = 0x0000_0004_0000_0003_0000_0002_0000_0001_u128;
    assert_eq!(iced.xmm_at(0), want, "iced pshufd");
    assert_eq!(jit.xmm_at(0), want, "jit pshufd");
    // Hand-check PSHUFB with mask 0x10 repeated → table[0] per byte.
    let table = 0x0100_0000_0000_0000_0000_0000_0000_0042_u128;
    let mask = 0x1010_1010_1010_1010_1010_1010_1010_1010_u128;
    let (iced, jit) = simd_dual(&[0x66, 0x0f, 0x38, 0x00, 0xc1], &[], |r| {
        set_pair(r, table, mask);
    });
    assert_eq!(
        iced.xmm_at(0),
        0x4242_4242_4242_4242_4242_4242_4242_4242,
        "iced pshufb"
    );
    assert_eq!(
        jit.xmm_at(0),
        0x4242_4242_4242_4242_4242_4242_4242_4242,
        "jit pshufb"
    );
    // Bit-7 in the mask → 0 for that byte.
    let mask = 0x8080_8080_8080_8080_8080_8080_8080_8080_u128;
    let (iced, jit) = simd_dual(&[0x66, 0x0f, 0x38, 0x00, 0xc1], &[], |r| {
        set_pair(r, table, mask);
    });
    assert_eq!(iced.xmm_at(0), 0, "iced pshufb bit7 zeroes");
    assert_eq!(jit.xmm_at(0), 0, "jit pshufb bit7 zeroes");
}

#[test]
fn simd_converts_match_iced() {
    // cvtsi2ss/cvtsi2sd from r32/r64.
    for (name, bytes) in [
        ("cvtsi2ss r32", &[0xf3, 0x0f, 0x2a, 0xc1][..]),
        ("cvtsi2ss r64", &[0xf3, 0x48, 0x0f, 0x2a, 0xc1][..]),
        ("cvtsi2sd r32", &[0xf2, 0x0f, 0x2a, 0xc1][..]),
        ("cvtsi2sd r64", &[0xf2, 0x48, 0x0f, 0x2a, 0xc1][..]),
    ] {
        let (iced, jit) = simd_dual(bytes, &[], |r| {
            set_pair(r, 0, 0);
            r.set_gpr_public(1, 0x0000_0000_0000_00FF); // RCX = 255
        });
        assert_same_regs(&iced, &jit, name);
    }
    // FP → int (both widths, truncating and rounding).
    for (name, bytes) in [
        ("cvttss2si r32", &[0xf3, 0x0f, 0x2c, 0xc0][..]),
        ("cvttss2si r64", &[0xf3, 0x48, 0x0f, 0x2c, 0xc0][..]),
        ("cvtss2si r32", &[0xf3, 0x0f, 0x2d, 0xc0][..]),
        ("cvttsd2si r64", &[0xf2, 0x48, 0x0f, 0x2c, 0xc0][..]),
        ("cvtsd2si r32", &[0xf2, 0x0f, 0x2d, 0xc0][..]),
    ] {
        let (iced, jit) = simd_dual(bytes, &[], |r| {
            let bits = 42.75_f32.to_bits();
            set_pair(r, u128::from(bits), 0);
            r.set_gpr_public(0, 0);
        });
        assert_same_regs(&iced, &jit, name);
    }
    // Hand-check CVTTSS2SI: 42.75 → 42 (truncate).
    let (iced, jit) = simd_dual(&[0xf3, 0x0f, 0x2c, 0xc0], &[], |r| {
        set_pair(r, u128::from(42.75_f32.to_bits()), 0);
        r.set_gpr_public(0, 0xdead_beef);
    });
    assert_eq!(iced.gpr(0), 42, "iced cvttss2si");
    assert_eq!(jit.gpr(0), 42, "jit cvttss2si");
    // Hand-check CVTTSS2SI on NaN → INT_MIN (0x80000000), 32-bit write zero-extends.
    let (iced, jit) = simd_dual(&[0xf3, 0x0f, 0x2c, 0xc0], &[], |r| {
        set_pair(r, u128::from(f32::NAN.to_bits()), 0);
        r.set_gpr_public(0, 0);
    });
    assert_eq!(iced.gpr(0), 0x8000_0000, "iced cvttss2si nan");
    assert_eq!(jit.gpr(0), 0x8000_0000, "jit cvttss2si nan");
    // Packed converts.
    for (name, bytes) in [
        ("cvtps2dq", &[0x66, 0x0f, 0x5b, 0xc1][..]),
        ("cvtdq2ps", &[0x0f, 0x5b, 0xc1][..]),
        ("cvttps2dq", &[0xf3, 0x0f, 0x5b, 0xc1][..]),
    ] {
        let (iced, jit) = simd_dual(bytes, &[], |r| {
            // four f32 lanes: 1.5, -2.5, 3.0, 4.25
            let f0 = 1.5_f32.to_bits();
            let f1 = (-2.5_f32).to_bits();
            let f2 = 3.0_f32.to_bits();
            let f3 = 4.25_f32.to_bits();
            let v = u128::from(f0)
                | (u128::from(f1) << 32)
                | (u128::from(f2) << 64)
                | (u128::from(f3) << 96);
            set_pair(r, v, 0);
        });
        assert_same_regs(&iced, &jit, name);
    }
}

#[test]
fn simd_fp_minmax_sqrt_match_iced() {
    let x0 = 0x3FF0_0000_0000_0000_3FE0_0000_3FC0_0000_u128;
    let x1 = 0x4000_0000_0000_0000_4000_0000_3F80_0000_u128;
    for (name, bytes) in [
        ("sqrtss", &[0xf3, 0x0f, 0x51, 0xc1][..]),
        ("sqrtsd", &[0xf2, 0x0f, 0x51, 0xc1][..]),
        ("sqrtps", &[0x0f, 0x51, 0xc1][..]),
        ("sqrtpd", &[0x66, 0x0f, 0x51, 0xc1][..]),
        ("minss", &[0xf3, 0x0f, 0x5d, 0xc1][..]),
        ("minsd", &[0xf2, 0x0f, 0x5d, 0xc1][..]),
        ("minps", &[0x0f, 0x5d, 0xc1][..]),
        ("minpd", &[0x66, 0x0f, 0x5d, 0xc1][..]),
        ("maxss", &[0xf3, 0x0f, 0x5f, 0xc1][..]),
        ("maxsd", &[0xf2, 0x0f, 0x5f, 0xc1][..]),
        ("maxps", &[0x0f, 0x5f, 0xc1][..]),
        ("maxpd", &[0x66, 0x0f, 0x5f, 0xc1][..]),
    ] {
        let (iced, jit) = simd_dual(bytes, &[], |r| set_pair(r, x0, x1));
        assert_same_regs(&iced, &jit, name);
    }
    // Hand-check SQRTSS of 4.0 → 2.0.
    let a = 0x0000_0000_0000_0000_0000_0000_0000_0000_u128;
    let b = 0x0000_0000_0000_0000_0000_0000_0000_0000_u128;
    let a = a | u128::from(123.456_f32.to_bits());
    let b = b | u128::from(4.0_f32.to_bits());
    let (iced, jit) = simd_dual(&[0xf3, 0x0f, 0x51, 0xc1], &[], |r| set_pair(r, a, b));
    let want = u64::from(2.0_f32.to_bits());
    assert_eq!(
        iced.xmm_at(0) & 0xffff_ffff,
        u128::from(want),
        "iced sqrtss"
    );
    assert_eq!(jit.xmm_at(0) & 0xffff_ffff, u128::from(want), "jit sqrtss");
    // MINSS with a NaN operand returns the source (x86 semantics).
    let nan = u128::from(f32::NAN.to_bits());
    let (iced, jit) = simd_dual(&[0xf3, 0x0f, 0x5d, 0xc1], &[], |r| set_pair(r, a | nan, b));
    let lo = iced.xmm_at(0) & 0xffff_ffff;
    assert_eq!(lo, b & 0xffff_ffff, "iced minss nan→src");
    let lo = jit.xmm_at(0) & 0xffff_ffff;
    assert_eq!(lo, b & 0xffff_ffff, "jit minss nan→src");
}

#[test]
fn simd_comis_sets_flags_like_iced() {
    let a32 = u128::from(1.0_f32.to_bits());
    let b32 = u128::from(2.0_f32.to_bits());
    for (name, bytes) in [
        ("comiss", &[0x0f, 0x2f, 0xc1][..]),
        ("ucomiss", &[0x0f, 0x2e, 0xc1][..]),
        ("comisd", &[0x66, 0x0f, 0x2f, 0xc1][..]),
        ("ucomisd", &[0x66, 0x0f, 0x2e, 0xc1][..]),
    ] {
        // Pre-set CF so we can see it being cleared.
        let (iced, jit) = simd_dual(bytes, &[], |r| {
            set_pair(r, a32, b32);
            r.rflags = Rflags::ALWAYS1 | Rflags::CF;
        });
        assert_same_regs(&iced, &jit, name);
        // a < b → CF=1, ZF=0, PF=0, OF/AF/SF=0.
        assert!(iced.flag(Rflags::CF), "{name} iced CF");
        assert!(!iced.flag(Rflags::ZF), "{name} iced ZF");
        assert!(!iced.flag(Rflags::OF), "{name} iced OF");
        assert!(jit.flag(Rflags::CF), "{name} jit CF");
        assert!(!jit.flag(Rflags::ZF), "{name} jit ZF");
        assert!(!jit.flag(Rflags::OF), "{name} jit OF");
    }
    // a == b → ZF=1, CF=0, PF=0.
    let (iced, jit) = simd_dual(&[0x0f, 0x2f, 0xc1], &[], |r| set_pair(r, b32, b32));
    assert!(iced.flag(Rflags::ZF), "iced eq ZF");
    assert!(!iced.flag(Rflags::CF), "iced eq CF");
    assert!(jit.flag(Rflags::ZF), "jit eq ZF");
    assert!(!jit.flag(Rflags::CF), "jit eq CF");
    // NaN → unordered: ZF=PF=CF=1.
    let (iced, jit) = simd_dual(&[0x0f, 0x2f, 0xc1], &[], |r| {
        set_pair(r, u128::from(f32::NAN.to_bits()), b32);
    });
    assert!(iced.flag(Rflags::PF), "iced nan PF");
    assert!(iced.flag(Rflags::CF), "iced nan CF");
    assert!(jit.flag(Rflags::PF), "jit nan PF");
    assert!(jit.flag(Rflags::CF), "jit nan CF");
}

#[test]
fn simd_movq_movd_gpr_bridge_matches_iced() {
    let x = 0x8899_AABB_CCDD_EEFF_0011_2233_4455_6677_u128;
    for (name, bytes) in [
        ("movq xmm,r64", &[0x66, 0x48, 0x0f, 0x6e, 0xc1][..]),
        ("movq r64,xmm", &[0x66, 0x48, 0x0f, 0x7e, 0xc1][..]),
        ("movd xmm,r32", &[0x66, 0x0f, 0x6e, 0xc1][..]),
        ("movd r32,xmm", &[0x66, 0x0f, 0x7e, 0xc1][..]),
    ] {
        let (iced, jit) = simd_dual(bytes, &[], |r| {
            set_pair(r, x, 0);
            r.set_gpr_public(1, 0x1234_5678_9ABC_DEF0); // RCX
        });
        assert_same_regs(&iced, &jit, name);
    }
    // Hand-check MOVQ xmm,r64: low qword moved, high 64 zeroed.
    let (iced, jit) = simd_dual(&[0x66, 0x48, 0x0f, 0x6e, 0xc1], &[], |r| {
        set_pair(r, u128::MAX, 0);
        r.set_gpr_public(1, 0x0011_2233_4455_6677);
    });
    let want = 0x0000_0000_0000_0000_0011_2233_4455_6677_u128;
    assert_eq!(iced.xmm_at(0), want, "iced movq zeroes hi");
    assert_eq!(jit.xmm_at(0), want, "jit movq zeroes hi");
    // Hand-check MOVQ r64,xmm: low qword extracted.
    let (iced, jit) = simd_dual(&[0x66, 0x48, 0x0f, 0x7e, 0xc1], &[], |r| {
        set_pair(r, 0x8899_AABB_CCDD_EEFF_0011_2233_4455_6677, 0);
        r.set_gpr_public(1, 0);
    });
    assert_eq!(iced.gpr(1), 0x0011_2233_4455_6677, "iced movq extract");
    assert_eq!(jit.gpr(1), 0x0011_2233_4455_6677, "jit movq extract");
    // MOVD zero-extends 32 bits.
    let (iced, jit) = simd_dual(&[0x66, 0x0f, 0x6e, 0xc1], &[], |r| {
        set_pair(r, u128::MAX, 0);
        r.set_gpr_public(1, 0x1234_5678_9ABC_DEF0);
    });
    let want = 0x0000_0000_0000_0000_0000_0000_9ABC_DEF0_u128;
    assert_eq!(iced.xmm_at(0), want, "iced movd zero-extends");
    assert_eq!(jit.xmm_at(0), want, "jit movd zero-extends");
}

#[test]
fn simd_memory_operands_match_iced() {
    let data = 0x0001_0002_0003_0004_0005_0006_0007_0008_u128.to_le_bytes();
    for (name, bytes) in [
        ("paddd [rax]", &[0x66, 0x0f, 0xfe, 0x00][..]),
        ("pshufb [rax]", &[0x66, 0x0f, 0x38, 0x00, 0x00][..]),
        ("comiss [rax]", &[0x0f, 0x2f, 0x00][..]),
        ("cvtsi2ss [rax]", &[0xf3, 0x0f, 0x2a, 0x00][..]),
    ] {
        let (iced, jit) = simd_dual(bytes, &data, |r| {
            set_pair(r, 0x0000_0004_0000_0003_0000_0002_0000_0001, 0);
            r.set_gpr_public(0, SIMD_DATA); // RAX = data base
        });
        assert_same_regs(&iced, &jit, name);
    }
    // Hand-check PADDD xmm0, [rax]: add dword0 of memory (0x00000001).
    let a = 0x0000_0000_0000_0000_0000_0000_0000_0005_u128;
    let data = 0x0000_0000_0000_0000_0000_0000_0000_0001_u128.to_le_bytes();
    let (iced, jit) = simd_dual(&[0x66, 0x0f, 0xfe, 0x00], &data, |r| {
        set_pair(r, a, 0);
        r.set_gpr_public(0, SIMD_DATA);
    });
    assert_eq!(
        iced.xmm_at(0),
        0x0000_0000_0000_0000_0000_0000_0000_0006,
        "iced paddd mem"
    );
    assert_eq!(
        jit.xmm_at(0),
        0x0000_0000_0000_0000_0000_0000_0000_0006,
        "jit paddd mem"
    );
}
