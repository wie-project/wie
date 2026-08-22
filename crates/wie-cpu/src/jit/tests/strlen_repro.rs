use super::chain_tests::chain_dual;
use super::*;
use crate::CpuEngine;
use crate::regs::RegFile;

/// ── strlen haszero differential #1: NUL-detection correctness ────
///
/// Doom Retro's section dispatch computes `strlen(key)` with the classic
/// word-at-a-time zero test:
///
/// ```text
/// r9  = 0x7efefefefefefeff + x
/// rdx = ~x
/// rdx = rdx ^ r9
/// rdx = rdx & 0x8101010101010100   ; nonzero ⇔ a NUL byte in x
/// ```
///
/// The failing DEHACKED compare walked past the entry NUL, which implies this
/// sequence mis-evaluates on one of the engines. Differential-test both qwords
/// of the literal `"[STRINGS]\0[PARS]\0"`.
#[test]
fn strlen_haszero_differential() {
    // "[STRINGS]\0[PARS]\0" — the .rdata layout at doomretro.exe 0x140183168.
    let buf = b"[STRINGS]\0[PARS]\0";

    let detect_code = || -> Vec<u8> {
        let mut c = vec![];
        // movabs r8, 0x7efefefefefefeff
        c.extend_from_slice(&[0x49, 0xB8, 0xFF, 0xFE, 0xFE, 0xFE, 0xFE, 0xFE, 0xFE, 0x7E]);
        // movabs r11, 0x8101010101010100
        c.extend_from_slice(&[0x49, 0xBB, 0x00, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x81]);
        // mov rdx, qword ptr [rcx]      ; rcx = &buf[offset]
        c.extend_from_slice(&[0x48, 0x8B, 0x11]);
        // mov r9, r8
        c.extend_from_slice(&[0x4D, 0x8B, 0xC8]);
        // add r9, rdx
        c.extend_from_slice(&[0x4C, 0x03, 0xCA]);
        // not rdx
        c.extend_from_slice(&[0x48, 0xF7, 0xD2]);
        // xor rdx, r9
        c.extend_from_slice(&[0x49, 0x33, 0xD1]);
        // and rdx, r11
        c.extend_from_slice(&[0x49, 0x23, 0xD3]);
        // ud2 — stop here; rdx holds the detection bits.
        c.extend_from_slice(&[0x0F, 0x0B]);
        c
    };

    for (label, offset, expect_zero_detected) in [
        ("qword0 \"[STRINGS\" (no NUL)", 0_u64, false),
        ("qword1 \"S\\0[PARS\" (NUL at byte 1)", 8_u64, true),
    ] {
        let code = detect_code();
        let stop_rip = SIMD_BASE + code.len() as u64 - 2; // ud2
        let buf_va = SIMD_DATA + 0x800;
        let setup_mem = |eng: &mut dyn CpuEngine| {
            eng.mem_write(buf_va, buf).expect("write buf");
        };
        let setup = |regs: &mut RegFile| {
            regs.set_gpr(1, buf_va + offset); // rcx → &buf[offset]
        };
        let (iced, jit) = chain_dual(&code, stop_rip, setup, setup_mem);
        assert_eq!(
            iced.gpr(2),
            jit.gpr(2),
            "{label}: detect bits diverge — iced={:#x} jit={:#x}",
            iced.gpr(2),
            jit.gpr(2)
        );
        if expect_zero_detected {
            assert_ne!(
                jit.gpr(2),
                0,
                "{label}: NUL NOT detected by JIT (detect bits == 0) → strlen overshoot"
            );
            assert_ne!(
                iced.gpr(2),
                0,
                "{label}: NUL NOT detected by iced (detect bits == 0)"
            );
        } else {
            assert_eq!(jit.gpr(2), 0, "{label}: false-positive NUL detected by JIT");
        }
    }
}

/// ── strlen haszero differential #2: lfstrip's actual input ────
///
/// `lfstrip("[STRINGS]\r\nVERSION = DOOM Retro v6.3\r\n")` runs the same
/// haszero load loop over this exact line. Every qword here is NUL-free until
/// past the end, so every detection result must be ZERO on both engines. A
/// nonzero result is a FALSE-POSITIVE NUL — it makes strlen stop early, which
/// leaves the trailing `\r` unstripped, which is exactly the Doom Retro
/// version-check failure (`"[STRINGS]\r"` never matches `"[STRINGS]"`).
#[test]
fn strlen_lfstrip_line_no_false_positive() {
    let line = b"[STRINGS]\r\nVERSION = DOOM Retro v6.3\r\n";

    let code = {
        let mut c = vec![];
        c.extend_from_slice(&[0x49, 0xB8, 0xFF, 0xFE, 0xFE, 0xFE, 0xFE, 0xFE, 0xFE, 0x7E]);
        c.extend_from_slice(&[0x49, 0xBB, 0x00, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x81]);
        c.extend_from_slice(&[0x48, 0x8B, 0x11]);
        c.extend_from_slice(&[0x4D, 0x8B, 0xC8]);
        c.extend_from_slice(&[0x4C, 0x03, 0xCA]);
        c.extend_from_slice(&[0x48, 0xF7, 0xD2]);
        c.extend_from_slice(&[0x49, 0x33, 0xD1]);
        c.extend_from_slice(&[0x49, 0x23, 0xD3]);
        c.extend_from_slice(&[0x0F, 0x0B]);
        c
    };

    let mut failures = Vec::new();
    for off in 0..=(line.len() - 8) {
        let buf_va = SIMD_DATA + 0x800;
        let stop_rip = SIMD_BASE + code.len() as u64 - 2;
        let setup_mem = |eng: &mut dyn CpuEngine| {
            eng.mem_write(buf_va, line).expect("write line");
        };
        let setup = |regs: &mut RegFile| {
            regs.set_gpr(1, buf_va + off as u64);
        };
        let (iced, jit) = chain_dual(&code, stop_rip, setup, setup_mem);

        // Host reference for this qword.
        let x = u64::from_le_bytes(line[off..off + 8].try_into().unwrap());
        let v = x.wrapping_add(0x7EFEFEFEFEFEFEFF);
        let expect = (v ^ !x) & 0x8101010101010100;

        if iced.gpr(2) != jit.gpr(2) {
            failures.push(format!(
                "off={off}: DIVERGE iced={:#x} jit={:#x} (host expect={expect:#x})",
                iced.gpr(2),
                jit.gpr(2)
            ));
        } else if iced.gpr(2) != expect {
            failures.push(format!(
                "off={off}: BOTH WRONG got={:#x} expect={expect:#x} (qword={x:#018x})",
                iced.gpr(2)
            ));
        } else if expect != 0
            && off + 8 <= line.len() - 1
            && line[off..off + 8].iter().all(|&b| b != 0)
        {
            failures.push(format!(
                "off={off}: FALSE-POSITIVE NUL detected (qword={x:#018x} has no NUL)"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "strlen haszero mis-executions:\n{}",
        failures.join("\n")
    );
}
