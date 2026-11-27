// Flag **consumers** read a flag a previous instruction in the same block set.
//
// The SSA `FlagState` (`lower::flags`) exists so a consumer reads an i1 instead
// of decoding the packed RFLAGS word, and the pending/written split
// (`PendingFlags` + `flush_pending`) exists so a consumer sitting between a
// flag-setting instruction and the next flag write sees the pending arithmetic
// rather than the stale packed word. Both properties are invisible in the
// instruction results unless the *condition* is wrong, and a wrong condition
// shows up as a mis-executed branch on a real guest long after the fact — so
// every case here is run twice, once on the iced interpreter (the reference) and
// once through a compiled Cranelift block, and the whole register file is
// compared (`assert_same_regs`, including RFLAGS and RIP).
//
// The consumer shapes are the ones the lowering special-cases: `cmovcc` and
// `setcc` (FLAGS→GPR inside the body), `jcc` (FLAGS→branch at the terminator),
// and `adc`/`sbb` (FLAGS→GPR *after a flush*, which is the pending-flag path).
// `imul` is here because it is an *eager* writer: it rewrites only CF/OF in the
// carrier, and a consumer after it must see those two without the state having
// been re-derived from the packed word.
//
// Split out of `simd_parity_tests` (which is the SSE family) rather than
// appended to it: this file is about integer flags, and it reuses that file's
// dual-engine harness instead of duplicating it.
use super::chain_tests::chain_dual;
use super::simd_parity_tests::{assert_same_regs, simd_dual};
use crate::exec::StepResult;
use crate::regs::{RegFile, Rflags};

/// Condition codes in encoding order: `0x70 | cc` is `jcc`, `0x40 | cc` is
/// `cmovcc`, `0x90 | cc` is `setcc`. Names are the iced mnemonics.
const COND_NAMES: [&str; 16] = [
    "o", "no", "b", "ae", "e", "ne", "be", "a", "s", "ns", "p", "np", "l", "ge", "le", "g",
];

/// `cmp eax, ebx` (39 /r) — the flag writer that heads every case. It sets
/// ZF/SF/CF/OF/AF/PF, so every condition below is a live consumer of state that
/// was produced *in this block*.
const CMP_EAX_EBX: [u8; 2] = [0x39, 0xD8];

/// `(name, eax, ebx)` — the `cmp eax, ebx` flag combinations the cases below
/// cycle through, chosen so each condition is exercised both taken and not
/// taken:
///
/// * equal               → ZF=1
/// * unsigned-less      → CF=1, SF=1, ZF=0
/// * signed-overflow    → OF=1, SF=0 (0x8000_0000 - 1)
/// * greater            → CF=0, SF=0, ZF=0
/// * negative-no-borrow → SF=1, CF=0, OF=0
/// * pf-odd             → result 1, PF=0
const CMP_CASES: [(&str, u64, u64); 6] = [
    ("equal", 5, 5),
    ("unsigned-less", 1, 2),
    ("signed-overflow", 0x8000_0000, 1),
    ("greater", 2, 1),
    ("negative-no-borrow", 0xffff_ffff, 0),
    ("pf-odd", 2, 1),
];

const INIT_ECX: u64 = 0x1111_2222;
const INIT_EDX: u64 = 0x3333_4444;

/// Guest state shared by every case: operands in EAX/EBX, `cmov`/`setcc`
/// destination and source in ECX/EDX.
///
/// `Copy` because [`chain_dual`] runs the closure on both engines.
fn setup(eax: u64, ebx: u64) -> impl Fn(&mut RegFile) + Copy {
    move |r: &mut RegFile| {
        r.set_gpr_public(0, eax);
        r.set_gpr_public(3, ebx);
        r.set_gpr_public(1, INIT_ECX);
        r.set_gpr_public(2, INIT_EDX);
    }
}

/// `cmovcc ecx, edx` — 0F 4x /r, ModRM 0xCA = ecx ← (edx if cc).
fn cmov_ecx_edx(cc: u8) -> [u8; 3] {
    [0x0F, 0x40 | cc, 0xCA]
}

/// `setcc cl` — 0F 9x /0, ModRM 0xC1.
fn setcc_cl(cc: u8) -> [u8; 3] {
    [0x0F, 0x90 | cc, 0xC1]
}

/// `cmovcc ecx, edx` reading the `cmp`'s flags, for all 16 conditions and all
/// six flag combinations.
///
/// The observable is ECX: it is `edx` exactly when the condition holds, so a
/// condition that reads the wrong flag (or a stale one) changes ECX instead of
/// silently taking the same branch.
#[test]
fn cmovcc_reads_flags_set_earlier_in_the_block() {
    for (case, eax, ebx) in CMP_CASES {
        for (cc, name) in COND_NAMES.iter().enumerate() {
            let cc = u8::try_from(cc).expect("0..16");
            let mut code = Vec::from(CMP_EAX_EBX);
            code.extend_from_slice(&cmov_ecx_edx(cc));
            let (iced, jit) = simd_dual(&code, &[], setup(eax, ebx));
            assert_same_regs(&iced, &jit, &format!("cmov{name} after cmp, {case}"));
        }
    }
}

/// `setcc cl` — the same table, one byte wide. CL is the low byte of RCX, so
/// this checks the zero/sign/carry/overflow/parity reads independently of the
/// operand-width handling that `cmov` exercises.
#[test]
fn setcc_reads_flags_set_earlier_in_the_block() {
    for (case, eax, ebx) in CMP_CASES {
        for (cc, name) in COND_NAMES.iter().enumerate() {
            let cc = u8::try_from(cc).expect("0..16");
            let mut code = Vec::from(CMP_EAX_EBX);
            code.extend_from_slice(&setcc_cl(cc));
            let (iced, jit) = simd_dual(&code, &[], setup(eax, ebx));
            assert_same_regs(&iced, &jit, &format!("set{name} after cmp, {case}"));
        }
    }
}

/// `jcc` as the block terminator, reading the `cmp` one instruction earlier.
///
/// Layout: `39 D8` (cmp) · `7x 01` (jcc +1) · `90` (nop) · `0F 0B` (ud2). Taken
/// leaves RIP at the ud2, not-taken at the nop, so RIP *is* the condition and
/// the comparison covers the branch as well as RFLAGS. Neither is executed: the
/// ud2 would stop the run.
#[test]
fn jcc_reads_flags_set_earlier_in_the_block() {
    for (case, eax, ebx) in CMP_CASES {
        for (cc, name) in COND_NAMES.iter().enumerate() {
            let cc = u8::try_from(cc).expect("0..16");
            let mut code = Vec::from(CMP_EAX_EBX);
            code.extend_from_slice(&[0x70 | cc, 0x01, 0x90, 0x0F, 0x0B]);
            // Stop on the first RIP at or past the `nop`.
            let stop = super::SIMD_BASE + 4;
            let (iced, jit) = chain_dual(&code, stop, setup(eax, ebx), |_| {});
            assert_same_regs(&iced, &jit, &format!("j{name} after cmp, {case}"));
            assert!(
                iced.rip == super::SIMD_BASE + 4 || iced.rip == super::SIMD_BASE + 5,
                "j{name}, {case}: rip {:#x} is neither the nop nor the ud2",
                iced.rip
            );
        }
    }
}

/// A block whose **successor** reads the flags it leaves behind.
///
/// Layout: `39 D8` (cmp) · `EB 00` (jmp +0) · `7x 01` (jcc +1) · `90` (nop) ·
/// `0F 0B` (ud2). The first block ends at the `jmp` without reading any flag;
/// the `jcc` that consumes them is the *next* block, reached through a chain hop
/// (or the dispatcher, if the successor is not compiled yet — both engines only
/// have to agree on where they end up). This is the path that keeps the packed
/// carrier alive across a block boundary, and the reason `rflags` is still
/// stored at block exit.
#[test]
fn successor_block_reads_flags_from_the_previous_blocks_cmp() {
    for (case, eax, ebx) in CMP_CASES {
        for (cc, name) in COND_NAMES.iter().enumerate() {
            let cc = u8::try_from(cc).expect("0..16");
            let mut code = Vec::from(CMP_EAX_EBX);
            // jmp +0 lands on the jcc; the jcc's +1 skips the nop to the ud2.
            code.extend_from_slice(&[0xEB, 0x00]);
            code.extend_from_slice(&[0x70 | cc, 0x01, 0x90, 0x0F, 0x0B]);
            let stop = super::SIMD_BASE + 6;
            let (iced, jit) = chain_dual(&code, stop, setup(eax, ebx), |_| {});
            assert_same_regs(&iced, &jit, &format!("j{name} after a jmp block, {case}"));
        }
    }
}

/// `adc`/`sbb` read the carry *after a flush*: they are the consumers that go
/// through `flush_pending`, so they see the pending `cmp` arithmetic and not the
/// packed word as it stood before it. ECX carries the result
/// (`ecx + ecx + CF` for `adc`, `ecx - ecx - CF` for `sbb`).
#[test]
fn adc_and_sbb_read_the_pending_carry() {
    for (case, eax, ebx) in CMP_CASES {
        for (name, op) in [("adc", [0x11_u8, 0xC9]), ("sbb", [0x19_u8, 0xC9])] {
            let mut code = Vec::from(CMP_EAX_EBX);
            code.extend_from_slice(&op);
            let (iced, jit) = simd_dual(&code, &[], setup(eax, ebx));
            assert_same_regs(&iced, &jit, &format!("{name} after cmp, {case}"));
        }
    }
}

/// The 64-bit `adc`/`sbb` link: `cmp rax,rbx` then `adc rcx,rcx` / `sbb rcx,rcx`.
///
/// The 64-bit carry-out is computed from the two half-adds (`bool_to_i64` over
/// the two wrap compares), which is a different path from the 32-bit `ushr`
/// form — so it needs its own rows. `ARITH_CASES` again, because CF must be
/// separated from OF for the borrow direction to be visible.
#[test]
fn adc64_and_sbb64_read_the_pending_carry() {
    for (case, eax, ebx) in ARITH_CASES {
        for (name, op) in [
            ("adc64", [0x48_u8, 0x11, 0xC9]),
            ("sbb64", [0x48, 0x19, 0xC9]),
        ] {
            let mut code = vec![0x48, 0x39, 0xD8]; // cmp rax, rbx
            code.extend_from_slice(&op);
            code.extend_from_slice(&setcc_cl(2)); // setb cl: the link's carry out
            let (iced, jit) = simd_dual(&code, &[], setup(eax, ebx));
            assert_same_regs(&iced, &jit, &format!("{name} after cmp, {case}"));
        }
    }
}

/// `imul` is an *eager* writer that defines only CF/OF. Its SSA state is now
/// updated from the overflow predicate directly, so these cases pin the two
/// flags it does define reaching a consumer in the same block, in both the
/// overflow and no-overflow direction.
///
/// `0x4000_0000 * 4` overflows 32 bits; `3 * 5` does not.
#[test]
fn consumer_reads_imuls_cf_and_of() {
    for (name, a, b) in [("overflow", 0x4000_0000_u64, 4_u64), ("no-overflow", 3, 5)] {
        for (cc, cname) in [(2_u8, "b"), (0_u8, "o"), (3_u8, "ae"), (1_u8, "no")] {
            let mut code = vec![0x0F, 0xAF, 0xC3]; // imul eax, ebx
            code.extend_from_slice(&cmov_ecx_edx(cc));
            let (iced, jit) = simd_dual(&code, &[], setup(a, b));
            assert_same_regs(&iced, &jit, &format!("cmov{cname} after imul, {name}"));

            let mut code = vec![0x0F, 0xAF, 0xC3];
            code.extend_from_slice(&setcc_cl(cc));
            let (iced, jit) = simd_dual(&code, &[], setup(a, b));
            assert_same_regs(&iced, &jit, &format!("set{cname} after imul, {name}"));
        }
    }
}

/// A `cmp`, then an `imul` (which flushes the pending `cmp` and rewrites only
/// CF/OF), then a `jne` that reads the *`cmp`'s* ZF.
///
/// This is the case where an eager writer must not disturb the flags it does not
/// define: ZF/SF/PF/AF still describe the `cmp`, so `jne` must branch on that.
/// Layout: `39 D8` (cmp) · `0F AF C9` (imul ecx, ecx) · `75 01` (jne +1) ·
/// `90` (nop) · `0F 0B` (ud2).
#[test]
fn jcc_after_an_imul_reads_the_earlier_cmp() {
    for (case, eax, ebx) in CMP_CASES {
        for (cc, name) in [(5_u8, "ne"), (4_u8, "e")] {
            let mut code = Vec::from(CMP_EAX_EBX);
            code.extend_from_slice(&[0x0F, 0xAF, 0xC9]);
            code.extend_from_slice(&[0x70 | cc, 0x01, 0x90, 0x0F, 0x0B]);
            let stop = super::SIMD_BASE + 8;
            let (iced, jit) = chain_dual(&code, stop, setup(eax, ebx), |_| {});
            assert_same_regs(&iced, &jit, &format!("j{name} after cmp+imul, {case}"));
        }
    }
}

// ── The arithmetic half: produced predicates and the packed word ────────
//
// The cases above pin the *consumers*. The tests below pin what feeds them,
// after the lowering started taking CF/OF from the host's own carry/overflow
// flags (`overflow_preds`) instead of re-deriving them from the operands, and
// started reusing a packed word while the SSA flag state has not moved
// (`FlagState::packed`). Both are invisible unless a flag is actually read, so
// every case is a full register-file differential against the interpreter.

/// Flag-writing instructions whose flags a following `setcc` reads. Each is
/// `dst ← dst op src` with dst=EAX, src=EBX, so the same operands feed every
/// row: `(name, bytes, operand width)`.
const ARITH_WRITERS: [(&str, [u8; 2], u32); 6] = [
    ("add32", [0x01, 0xD8], 32),
    ("sub32", [0x29, 0xD8], 32),
    ("add64", [0x48, 0x01], 64),
    ("sub64", [0x48, 0x29], 64),
    // `inc`/`dec` are the `keep_cf` path: they define every flag *except* CF.
    ("inc32", [0xFF, 0xC0], 32),
    ("dec32", [0xFF, 0xC8], 32),
];

/// Operand pairs for the *arithmetic* writers, chosen so that CF and OF are
/// **separated**: a table where every case has CF = OF = 0 cannot tell the two
/// predicates apart, and a mutation that swaps them passes. Measured — that is
/// exactly how the first version of this table missed one.
///
/// `(name, dst, src)`; comments give `add` CF/OF and `sub` CF/OF.
const ARITH_CASES: [(&str, u64, u64); 6] = [
    ("equal", 5, 5),                         // add 0/0   sub 0/0
    ("carry-not-overflow", 0xffff_ffff, 1),  // add 1/0 sub 0/0
    ("overflow-not-carry", 0x7fff_ffff, 1),  // add 0/1 sub 0/0
    ("borrow", 0, 1),                        // add 0/0   sub 1/0
    ("sign-both", 0x8000_0000, 0x8000_0000), // add 1/1 sub 0/1
    ("wide", 1, 0xffff_ffff),                // add 1/0   sub 1/0
];

/// `pushfq`/`pop` need a stack. The harness's data page is mapped and unused by
/// these fixtures, so half of it is the stack; every push is balanced by a pop.
const TEST_STACK: u64 = super::SIMD_DATA + 0x800;

/// [`setup`], plus a valid RSP for the `pushfq` cases.
fn setup_with_stack(eax: u64, ebx: u64) -> impl Fn(&mut RegFile) + Copy {
    move |r: &mut RegFile| {
        setup(eax, ebx)(r);
        r.set_gpr_public(4, TEST_STACK);
    }
}

/// `setcc cl` for all 16 conditions after each arithmetic writer shape.
///
/// This is the table that pins the *produced* predicates: CF comes from
/// `uadd_overflow`/`usub_overflow` (`adds`/`subs` + `cset`) and OF from the
/// signed forms, for 32- and 64-bit operands; CF on `inc`/`dec` must survive
/// untouched. A predicate that disagrees with the interpreter on any operand
/// pair changes CL.
#[test]
fn arithmetic_writers_feed_every_condition() {
    for (writer, bytes, width) in ARITH_WRITERS {
        for (case, eax, ebx) in ARITH_CASES {
            // Carry-in makes CF/borrow direction observable for inc/dec too.
            // Set it with admitted instructions only (`stc`/`clc` are not
            // JIT-lowered — they force the interpreter fallback): `xor` clears
            // CF, and `sub ecx,1` from zero borrows.
            for carry_in in [0_u8, 1] {
                let mut code = if carry_in == 1 {
                    vec![0x31, 0xC9, 0x83, 0xE9, 0x01] // xor ecx,ecx; sub ecx,1
                } else {
                    vec![0x31, 0xC9] // xor ecx,ecx
                };
                code.extend_from_slice(&bytes);
                if width == 64 {
                    // `48 01 /r` is `add r/m64, r64`: the ModRM byte is the second
                    // half of the two-byte entry above.
                    code.push(0xD8);
                }
                for (cc, name) in COND_NAMES.iter().enumerate() {
                    let cc = u8::try_from(cc).expect("0..16");
                    let mut case_code = code.clone();
                    case_code.extend_from_slice(&setcc_cl(cc));
                    let (iced, jit) = simd_dual(&case_code, &[], setup(eax, ebx));
                    assert_same_regs(
                        &iced,
                        &jit,
                        &format!("set{name} after {writer}, {case}, carry_in={carry_in}"),
                    );
                }
            }
        }
    }
}

/// `jcc` on the carry and overflow of the same writers — the branch form, which
/// reads the same i1s through a different consumer.
#[test]
fn arithmetic_writers_drive_every_branch() {
    for (writer, bytes, width) in ARITH_WRITERS {
        for (case, eax, ebx) in ARITH_CASES {
            for (cc, name) in COND_NAMES.iter().enumerate() {
                let cc = u8::try_from(cc).expect("0..16");
                let mut code = Vec::from(bytes);
                if width == 64 {
                    code.push(0xD8);
                }
                code.extend_from_slice(&[0x70 | cc, 0x01, 0x90, 0x0F, 0x0B]);
                let stop = super::SIMD_BASE + u64::try_from(code.len()).expect("len");
                let (iced, jit) = chain_dual(&code, stop, setup(eax, ebx), |_| {});
                assert_same_regs(&iced, &jit, &format!("j{name} after {writer}, {case}"));
            }
        }
    }
}

/// `pushfq` is the boundary that hands the **whole word** to the guest: it is
/// the consumer that forces a pack, so a flag bit that is computed right but
/// never inserted into the carrier shows up here and nowhere else.
///
/// `pushfq`/`pop rcx` keeps the stack balanced, so RCX *is* the architectural
/// word the block produced.
#[test]
fn pushfq_hands_out_the_packed_word() {
    for (writer, bytes, width) in ARITH_WRITERS {
        for (case, eax, ebx) in ARITH_CASES {
            let mut code = Vec::from(bytes);
            if width == 64 {
                code.push(0xD8);
            }
            code.extend_from_slice(&[0x9C, 0x59]); // pushfq; pop rcx
            let (iced, jit) = simd_dual(&code, &[], setup_with_stack(eax, ebx));
            assert_same_regs(
                &iced,
                &jit,
                &format!("pushfq after {writer}, {case}: the packed word differs"),
            );
        }
    }
}

/// Two writers, two packs, one block: `add` → `pushfq` → `sub` → `pushfq`.
///
/// `FlagState::packed` memoises the word "until the fields move", so a mutator
/// that forgets to invalidate it hands the *first* word to the second `pushfq`.
/// The two words must differ exactly as the interpreter's do — RCX and RDX are
/// compared, so a stale memo cannot pass.
#[test]
fn the_packed_word_is_rebuilt_after_a_second_writer() {
    for (case, eax, ebx) in CMP_CASES {
        // add eax,ebx · pushfq · pop rcx · sub eax,ebx · pushfq · pop rdx
        let code = [0x01, 0xD8, 0x9C, 0x59, 0x29, 0xD8, 0x9C, 0x5A];
        let (iced, jit) = simd_dual(&code, &[], setup_with_stack(eax, ebx));
        assert_same_regs(&iced, &jit, &format!("two packs, one block, {case}"));
    }
}

// ── Where the word is *built*: the block's exit edges ────────────────────
//
// `FlagState` is the authority inside a block, so the packed word is an
// observation that only has to exist where it can be seen: a helper call, a
// `pushfq`, or an edge out of the compiled code. A block that writes flags
// before it reads them therefore carries nothing across its own backedge, and a
// self-loop rebuilds the word once — on the edge that leaves the loop — instead
// of once per iteration. The tests below pin the two places that could go wrong
// when the word stops being maintained eagerly: the exit edge, and the loop
// header parameter that a *reader* at the top of the body depends on.

/// [`setup`] plus a valid RSP and an explicit incoming RFLAGS, so the bits the
/// loop body does not define (CF for `dec`, DF/IF/reserved for everything) can be
/// given a value the body cannot produce.
/// The harness's default word (IF + the reserved bit 1) and the same word with
/// CF set — CF is the bit the loop bodies below either define or must preserve,
/// so it has to differ between rows.
const LOOP_FLAGS: [u64; 2] = [0x202, 0x203];

fn setup_loop_with_flags(eax: u64, ebx: u64, rflags: u64) -> impl Fn(&mut RegFile) + Copy {
    move |r: &mut RegFile| {
        setup_with_stack(eax, ebx)(r);
        r.rflags = Rflags::from(rflags);
    }
}

/// A self-loop whose body **writes** the flags before anything reads them.
///
/// `dec eax` · `jne` back to itself. Nothing reads the incoming word, so the SSA
/// state starts from the word loaded once at entry and the backedge carries no
/// carrier at all — the only pack is on the edge that leaves the loop. Two
/// things are observable: `jne` must read the *last* `dec` (so the loop exits
/// after the right number of iterations), and the word at the exit must be the
/// loop's, with CF preserved from before the loop (which `dec` does not define)
/// and ZF/SF/OF/PF/AF from the final iteration. That last part is what a
/// `FlagState` started from a constant zero would get wrong.
///
/// `sub` is the second row because it *does* define CF, so the same fixture
/// pins the direction the two writers move CF in.
#[test]
fn self_loop_that_writes_flags_packs_on_the_exit_edge() {
    // Always an architectural word: bit 1 is reserved-1, so injecting a bare
    // `1` would test the harness's writeback (`set_rflags_checked` re-asserts it)
    // instead of the lowering.
    for (writer, bytes, rflags) in [
        // `dec eax` (FF C8) keeps CF; `sub eax, 1` (83 E8 01) borrows.
        ("dec", &[0xFF_u8, 0xC8][..], LOOP_FLAGS[1]),
        ("sub", &[0x83, 0xE8, 0x01][..], LOOP_FLAGS[1]),
        ("dec-no-carry-in", &[0xFF, 0xC8][..], LOOP_FLAGS[0]),
    ] {
        let mut code = Vec::from(bytes);
        // jne back to the loop head: the body is `bytes` long, then `75 rel8`.
        let body = u8::try_from(bytes.len()).expect("short body");
        let rel = 0_u8.wrapping_sub(body).wrapping_sub(2);
        code.extend_from_slice(&[0x75, rel]);
        code.extend_from_slice(&[0x90, 0x0F, 0x0B]); // nop, ud2
        let exit = u64::try_from(code.len() - 3).expect("len");
        let stop = super::SIMD_BASE + exit;
        // Small counters only: `chain_dual` steps iced once per instruction with
        // a 500-step cap, so a loop body has to fit inside it.
        for eax in [1_u64, 2, 5] {
            let (iced, jit) =
                chain_dual(&code, stop, setup_loop_with_flags(eax, 0, rflags), |_| {});
            assert_same_regs(
                &iced,
                &jit,
                &format!("self-loop {writer}, eax={eax:#x}, rflags={rflags:#x}"),
            );
        }
    }
}

/// A self-loop whose body **reads** the flags before it writes them: the loop
/// header parameter exists for exactly this shape.
///
/// `setb dl` reads the incoming CF; `xor ecx,ecx` then `cmp ecx, 1` leave CF=1
/// for the *next* iteration; `dec ebx` counts down (it does not define CF) and
/// `jne` leaves on ZF. So iteration 1 reads the CF the *dispatch* handed the
/// block (0 here, from [`LOOP_FLAGS`][0]) and every later iteration reads the CF
/// the previous iteration's `cmp` left (1). With the carrier crossing the
/// backedge, DL ends at 1; without it, `setb` would read the loop's entry CF
/// every iteration and DL would end at 0. DL is compared, so the difference is
/// the assertion. Measured: removing the carrier (`flags_live_in` forced false)
/// flips the JIT's DL to 0 while iced stays at 1 — this is the mutation that
/// makes the test earn its keep.
#[test]
fn self_loop_that_reads_flags_carries_them_across_the_backedge() {
    let code = [
        0x0F, 0x92, 0xC2, // setb dl        reads the incoming CF
        0x31, 0xC9, // xor ecx, ecx   CF = 0
        0x83, 0xF9, 0x01, // cmp ecx, 1     CF = 1 for the next iteration
        0xFF, 0xCB, // dec ebx        (keeps CF)
        0x75, 0xF4, // jne back to the `setb`
        0x90, // nop
        0x0F, 0x0B, // ud2
    ];
    let stop = super::SIMD_BASE + 12;
    for ebx in [1_u64, 2, 3, 7] {
        for rflags in LOOP_FLAGS {
            let (iced, jit) =
                chain_dual(&code, stop, setup_loop_with_flags(0, ebx, rflags), |_| {});
            assert_same_regs(
                &iced,
                &jit,
                &format!("flag-reading self-loop, ebx={ebx}, rflags={rflags:#x}"),
            );
        }
    }
}

/// The exit edge of a self-loop into code that is **not compiled**: the word has
/// nowhere lazy to go, so the edge must pack it.
///
/// `dec eax` · `jne` self · `stc` · `pushfq` · `pop rdx` · `ud2`. `stc` is not
/// JIT-lowered (it forces the interpreter fallback), so the loop's exit edge
/// hands its word to the dispatcher and iced executes `stc` on it — the one exit
/// path where a lazy carrier must materialise. `stc` changes only CF, so every
/// other bit in RDX is the word the loop's last `dec` produced; a stale or
/// missing pack shows up there instead of in CF.
#[test]
fn self_loop_exit_into_the_interpreter_packs_the_word() {
    let code = [
        0xFF, 0xC8, // dec eax
        0x75, 0xFC, // jne back to it
        0xF9, // stc            (interpreter fallback)
        0x9C, // pushfq
        0x5A, // pop rdx
        0x0F, 0x0B, // ud2
    ];
    let stop = super::SIMD_BASE + 7;
    for eax in [1_u64, 3, 5] {
        for rflags in LOOP_FLAGS {
            let (iced, jit) =
                chain_dual(&code, stop, setup_loop_with_flags(eax, 0, rflags), |_| {});
            assert_same_regs(
                &iced,
                &jit,
                &format!("loop exit into the interpreter, eax={eax:#x}, rflags={rflags:#x}"),
            );
        }
    }
}

// ── The fault edge: EFLAGS as the guest observes them at a fault ────────
//
// A faulting memory access hands the guest its architectural flags, and the
// compiled block hands them over as a *value* (the exit block's `rflags`
// parameter). That makes a fault the one place where a flag writer that is still
// pending — assigned to `FlagState`, not yet materialised into the carrier —
// would be observable as a **wrong answer** rather than a missing optimisation:
// an SEH handler, a debugger, or a `pushfq` after the fault is handled would see
// the flags of whatever last happened to be packed.
//
// Every shape below puts the writer *before* the faulting access and its only
// consumer *after* it, so the consumer never runs and nothing in the block forces
// the carrier to exist for the block's own reasons. That is the leak, and each
// test also asserts the block really ran compiled (`iced_insns == 0`) so a silent
// interpreter fallback cannot make it pass for the wrong reason.

use super::harness::alloc_code;
use crate::{CpuEngine, IcedCpu, JitCpu};

/// An address no test engine maps: the fault has to come from the guest access,
/// not from a permission the harness happened to set up.
const UNMAPPED: u64 = 0xdead_0000_0000;

/// A `ThreadContext` for one fault case: operands for the flag writer, the
/// faulting base register, and an incoming word whose CF is the *opposite* of
/// what the writer produces — so a carrier left over from the entry state shows
/// up as a wrong CF rather than as a coincidentally equal one.
fn fault_ctx(code_va: u64, eax: u64, edx: u64, rsp: u64) -> crate::ThreadContext {
    let mut gpr = [0_u64; 16];
    gpr[0] = eax; // RAX — `add eax, edx` dst
    gpr[2] = edx; // RDX — `add eax, edx` src
    gpr[3] = UNMAPPED; // RBX — the explicit faulting base
    gpr[4] = rsp; // RSP — for the implicit stack shapes
    crate::ThreadContext {
        rip: code_va,
        gpr,
        rflags: Rflags::from(LOOP_FLAGS[1]), // CF=1: `add 0, 0` clears it
        ..crate::ThreadContext::default()
    }
}

/// Both engines, set up on the same bytes (each at its own code VA — the decode
/// cache rule), and run to the fault.
///
/// iced steps `pre` + 1 instructions so it stops *on* the faulting one; the JIT
/// runs the block once, which is the whole sequence. Returns the RFLAGS word and
/// the fault from each engine.
fn fault_dual(
    code: &[u8],
    eax: u64,
    edx: u64,
    rsp: u64,
    iced_steps: usize,
    what: &str,
) -> (Rflags, Rflags, StepResult, StepResult) {
    let mut iced = IcedCpu::open_x86_64();
    let mut jit = JitCpu::open_x86_64();
    let iced_va = alloc_code(&mut iced, code);
    let jit_va = alloc_code(&mut jit, code);
    iced.restore_thread_context(&fault_ctx(iced_va, eax, edx, rsp));
    jit.restore_thread_context(&fault_ctx(jit_va, eax, edx, rsp));
    jit.on_thread_switch();

    let mut iced_fault = StepResult::Continue;
    for _ in 0..=iced_steps {
        iced_fault = match iced.step_once_result() {
            Ok(r) => r,
            Err(e) => panic!("{what}: iced step: {e}"),
        };
        if !matches!(iced_fault, StepResult::Continue) {
            break;
        }
    }
    let (jit_fault, _retired) = match jit.step_one() {
        Ok(v) => v,
        Err(e) => panic!("{what}: jit step: {e}"),
    };
    assert_eq!(
        jit.stats().exec.iced_insns,
        0,
        "{what}: the JIT fell back to the interpreter, so this case proves nothing"
    );

    assert!(
        matches!(iced_fault, StepResult::InvalidMemory(_)),
        "{what}: iced did not fault, got {iced_fault:?}"
    );
    assert!(
        matches!(jit_fault, StepResult::InvalidMemory(_)),
        "{what}: the JIT did not fault, got {jit_fault:?}"
    );
    let iced_regs = iced.snapshot_thread_context();
    let jit_regs = jit.snapshot_thread_context();
    for i in 0..16 {
        assert_eq!(
            iced_regs.gpr[i], jit_regs.gpr[i],
            "{what}: gpr[{i}] at the fault"
        );
    }
    (iced_regs.rflags, jit_regs.rflags, iced_fault, jit_fault)
}

/// Assert the two words agree, naming the case. Split out because every shape
/// below compares exactly this and nothing else.
fn assert_fault_word(iced: Rflags, jit: Rflags, what: &str) {
    assert_eq!(
        u64::from(iced),
        u64::from(jit),
        "{what}: RFLAGS at the fault must be the writer's, not a stale carrier \
         (iced {:#x}, jit {:#x})",
        u64::from(iced),
        u64::from(jit)
    );
}

/// A fault **before** the block's first writer.
///
/// This is the shape the entry-decode gate creates: a block whose only flag
/// writes are full writers (`add`) and which reads no individual bit keeps the
/// incoming word *carried* rather than decoded, so at a fault nothing has been
/// assigned to the state and the reported word has to be the carried word —
/// bit for bit, including the DF/IF/reserved bits the lowering does not track. A
/// state whose six fields are placeholders must therefore never be *packed*;
/// only the memo of the word it came from may be handed out.
///
/// `mov ecx,[rbx]` faults before `add eax,edx` runs, and iced stops on it.
#[test]
fn a_fault_before_the_blocks_first_writer_reports_the_carried_word() {
    // mov ecx,[rbx] · add eax,edx · ud2
    let code = [0x8B, 0x0B, 0x01, 0xD0, 0x0F, 0x0B];
    for (case, eax, edx) in ARITH_CASES {
        let (iced, jit, _, _) = fault_dual(&code, eax, edx, 0, 0, case);
        assert_fault_word(
            iced,
            jit,
            &format!("fault before the block's first writer, {case}"),
        );
    }
}

/// A pending writer whose flags nothing in the block consumes: `add eax, edx`
/// then a faulting `mov ecx, [rbx]`.
#[test]
fn a_fault_after_a_pending_writer_reports_that_writers_flags() {
    // add eax, edx · mov ecx,[rbx] · ud2
    let code = [0x01, 0xD0, 0x8B, 0x0B, 0x0F, 0x0B];
    for (case, eax, edx) in ARITH_CASES {
        let (iced, jit, _, _) = fault_dual(&code, eax, edx, 0, 1, case);
        assert_fault_word(iced, jit, &format!("pending add, never consumed, {case}"));
    }
}

/// The same, with the writer's only consumer *after* the faulting instruction:
/// the `setb cl` would read CF, and the fault means it never runs.
#[test]
fn a_fault_between_a_writer_and_its_consumer_reports_the_writers_flags() {
    // add eax, edx · mov ecx,[rbx] · setb cl · ud2
    let code = [0x01, 0xD0, 0x8B, 0x0B, 0x0F, 0x92, 0xC1, 0x0F, 0x0B];
    for (case, eax, edx) in ARITH_CASES {
        let (iced, jit, _, _) = fault_dual(&code, eax, edx, 0, 1, case);
        assert_fault_word(
            iced,
            jit,
            &format!("pending add, consumer after the fault, {case}"),
        );
    }
}

/// A `push` after a pending writer: the stack access is implicit (no memory
/// operand kind), so this is the case an explicit-operand rule would miss.
#[test]
fn a_fault_on_an_implicit_stack_access_reports_the_writers_flags() {
    // add eax, edx · push rbx · ud2
    let code = [0x01, 0xD0, 0x53, 0x0F, 0x0B];
    for (case, eax, edx) in ARITH_CASES {
        let (iced, jit, _, _) = fault_dual(&code, eax, edx, UNMAPPED, 1, case);
        assert_fault_word(
            iced,
            jit,
            &format!("pending add, then a stack fault, {case}"),
        );
    }
}

/// A fault on the **terminator's** memory access: `ret` pops the return address,
/// so a pending writer before it is the case where a deferred body flush would
/// hand the fault a stale word.
#[test]
fn a_fault_on_the_terminators_stack_access_reports_the_writers_flags() {
    // add eax, edx · ret
    let code = [0x01, 0xD0, 0xC3];
    for (case, eax, edx) in ARITH_CASES {
        let (iced, jit, _, _) = fault_dual(&code, eax, edx, UNMAPPED, 1, case);
        assert_fault_word(
            iced,
            jit,
            &format!("pending add, then the ret's fault, {case}"),
        );
    }
}

// --- adc/sbb: the carry-in is read from the pending predicate, not the word ---
//
// `adc`/`sbb` used to force a full `flush_pending`: apply the previous writer's
// six flag predicates to the SSA state, pack the carrier (fourteen instructions),
// read one bit back out of the word it had just written, then pack *again* for
// its own result. Both packs sat on the block body. They now read CF from the
// pending predicate (`flags::pending_cf`) and leave their own flags pending, so
// nothing is packed between the links of a chain.
//
// The rows below are **chains** — writer, `adc`, `setc`, `sbb`, `setc` — because
// the property that has to hold is not "the carry is right once" but "each
// link's carry-in is the previous link's carry-out". `setc` writes no flags, so
// it observes a link's carry-out without breaking the chain, and each link's
// answer lands in a GPR where `assert_same_regs` sees it as a value, not just as
// a compared RFLAGS.

/// `adc esi, esi` — 11 /r, ModRM 0xF6 (mod 11, esi, esi).
const ADC_ESI_ESI: [u8; 2] = [0x11, 0xF6];
/// `sbb edi, edi` — 19 /r, ModRM 0xFF.
const SBB_EDI_EDI: [u8; 2] = [0x19, 0xFF];
/// `setc dl` — 0F 92 /0.
const SETC_DL: [u8; 3] = [0x0F, 0x92, 0xC2];
/// `setc cl` — 0F 92 /0.
const SETC_CL: [u8; 3] = [0x0F, 0x92, 0xC1];

/// `(name, writer)` — the flag writer whose **pending** state each link reads.
///
/// One row per `PendingFlags` variant a carry-in can meet: `Sub` (cmp), `Add`,
/// `Logic` (CF = 0 by definition), `Inc` (preserves the incoming CF), and `Shift`
/// (the one arm `pending_cf` cannot derive itself, so it flushes without
/// packing). `edi`/`esi` cycle through [`LINK_OPERANDS`].
const CARRY_WRITERS: [(&str, [u8; 2]); 5] = [
    ("cmp (pending sub)", [0x39, 0xD8]),
    ("add (pending add)", [0x01, 0xD8]),
    ("and (pending logic)", [0x21, 0xD8]),
    ("inc (preserves cf)", [0xFF, 0xC0]),
    ("shl (pending shift)", [0xD1, 0xE0]),
];

/// `(esi, edi)` — the chain's own operands, chosen so the two observed
/// carry-outs disagree: `adc esi,esi` carries out of `0x7fff_ffff`/`0xffff_ffff`
/// and not out of `1`; `sbb edi,edi` borrows out of `0`.
const LINK_OPERANDS: [(u64, u64); 3] = [(0x7fff_ffff, 0), (0xffff_ffff, 0xffff_ffff), (1, 1)];

/// [`setup`], with the chain's operands in ESI/EDI and the two observation
/// destinations cleared so `setc dl` / `setc cl` leave a readable 0/1.
fn setup_chain(eax: u64, ebx: u64, esi: u64, edi: u64) -> impl Fn(&mut RegFile) + Copy {
    move |r: &mut RegFile| {
        setup(eax, ebx)(r);
        r.set_gpr_public(6, esi);
        r.set_gpr_public(7, edi);
        r.set_gpr_public(1, 0);
        r.set_gpr_public(2, 0);
    }
}

/// Every pending writer's carry-in, through a two-link chain.
///
/// The writer defines the flags; `adc esi,esi` consumes its CF and produces a
/// new one; `setc dl` pins that; `sbb edi,edi` consumes *that* CF (so the second
/// link proves the first link's carry-out reached the pending state, not just a
/// register); `setc cl` pins the borrow. RFLAGS at the stop is the second link's
/// word, so the SBB flag set is compared too.
#[test]
fn adc_and_sbb_chain_each_links_carry_through_the_pending_state() {
    for (writer_name, writer) in CARRY_WRITERS {
        for (case, eax, ebx) in ARITH_CASES {
            for (esi, edi) in LINK_OPERANDS {
                let mut code = Vec::from(writer);
                code.extend_from_slice(&ADC_ESI_ESI);
                code.extend_from_slice(&SETC_DL);
                code.extend_from_slice(&SBB_EDI_EDI);
                code.extend_from_slice(&SETC_CL);
                let (iced, jit) = simd_dual(&code, &[], setup_chain(eax, ebx, esi, edi));
                assert_same_regs(
                    &iced,
                    &jit,
                    &format!(
                        "{writer_name} -> adc -> sbb chain, {case}, esi={esi:#x} edi={edi:#x}"
                    ),
                );
            }
        }
    }
}

/// Two `adc` links with **nothing between them**.
///
/// The chain above puts a `setc` between the links, and a `setc` is a consumer:
/// it flushes the pending state, so a second link that read CF from the state
/// instead of from the pending predicate would still see the right bit. This row
/// removes the consumer, which is the shape that decides whether the carry is
/// read from the *predicate* or from the last materialised word. `adc edi,edi`
/// takes the carry into its own result, so a wrong carry-in changes EDI, and
/// `setc cl` still pins the second link's carry-out.
#[test]
fn adc_links_carry_into_each_other_with_no_consumer_between() {
    for (writer_name, writer) in CARRY_WRITERS {
        for (case, eax, ebx) in ARITH_CASES {
            for (esi, edi) in LINK_OPERANDS {
                let mut code = Vec::from(writer);
                code.extend_from_slice(&ADC_ESI_ESI); // 11 F6
                code.extend_from_slice(&[0x11, 0xFF]); // adc edi, edi
                code.extend_from_slice(&SETC_CL);
                let (iced, jit) = simd_dual(&code, &[], setup_chain(eax, ebx, esi, edi));
                assert_same_regs(
                    &iced,
                    &jit,
                    &format!(
                        "{writer_name} -> adc -> adc (no consumer between), {case}, \
                     esi={esi:#x} edi={edi:#x}"
                    ),
                );
            }
        }
    }
}

/// A consumer that reads an **inherited** flag bit before this block writes flags.
///
/// This is the shape the entry-decode mask narrows: nothing here writes a flag
/// before `cmovb`/`cmovae` reads CF, so the entry state has to carry CF — and
/// *only* CF — from the incoming word. `add` after it writes all six, so the mask
/// stays narrow rather than becoming the whole cone.
///
/// The rows differ in the incoming CF, so a lowering that decoded the wrong bit
/// (or decoded nothing and left the field false) changes ECX instead of passing
/// silently. This is the fixture that makes the mask's *bit selection* observable
/// in a register: the analysis table can only check the mask itself.
#[test]
fn cmovcc_reads_an_inherited_carry_before_any_writer() {
    for (flags_name, flags) in [("cf clear", LOOP_FLAGS[0]), ("cf set", LOOP_FLAGS[1])] {
        for (cc, name) in [(2_u8, "cmovb"), (3, "cmovae")] {
            // cmovcc ecx, edx · add eax, 1 · nop · ud2
            let mut code = Vec::from(cmov_ecx_edx(cc));
            code.extend_from_slice(&[0x83, 0xC0, 0x01]);
            let setup = move |r: &mut RegFile| {
                setup(0, 0)(r);
                r.set_gpr_public(1, 0xaaaa); // ECX: the destination
                r.set_gpr_public(2, 0x5555); // EDX: the source
                r.rflags = Rflags::from(flags);
            };
            let (iced, jit) = simd_dual(&code, &[], setup);
            assert_same_regs(
                &iced,
                &jit,
                &format!("{name} on the inherited {flags_name}, before any writer"),
            );
        }
    }
}

/// A pending **logic** writer clears CF, whatever the block inherited.
///
/// AND/OR/XOR/TEST set CF = 0 unconditionally, and the incoming word here has
/// CF = 1 — so a carry-in read from the state (the last materialised word, or
/// the word the block was entered with) instead of from the logic writer's own
/// predicate gets this row wrong. The chain's operands are chosen so a spurious
/// carry changes the result *and* the carry-out.
#[test]
fn a_pending_logic_writer_clears_the_carry_a_chain_would_inherit() {
    // and eax,ebx · adc esi,esi · setc dl · adc edi,edi · setc cl
    let writers: [(&str, [u8; 2]); 3] = [
        ("and", [0x21, 0xD8]),
        ("or", [0x09, 0xD8]),
        ("xor", [0x31, 0xD8]),
    ];
    for (name, writer) in writers {
        for (esi, edi) in LINK_OPERANDS {
            for (flags_name, flags) in [("cf clear", LOOP_FLAGS[0]), ("cf set", LOOP_FLAGS[1])] {
                let mut code = Vec::from(writer);
                code.extend_from_slice(&ADC_ESI_ESI);
                code.extend_from_slice(&SETC_DL);
                code.extend_from_slice(&[0x11, 0xFF]); // adc edi,edi
                code.extend_from_slice(&SETC_CL);
                let setup = move |r: &mut RegFile| {
                    setup_chain(0xffff_ffff, 1, esi, edi)(r);
                    r.rflags = Rflags::from(flags);
                };
                let (iced, jit) = simd_dual(&code, &[], setup);
                assert_same_regs(
                    &iced,
                    &jit,
                    &format!(
                        "pending {name} with incoming {flags_name}, esi={esi:#x} edi={edi:#x}"
                    ),
                );
            }
        }
    }
}

/// The incoming CF through a chain that **preserves** it: `stc`-driven rows.
///
/// `inc`/`dec` and a zero-count shift define five flags and preserve CF, so their
/// carry-in to an `adc` is whatever the block inherited. Driven from RFLAGS
/// rather than from an instruction, because no instruction in the fixture sets CF
/// to a value the row can choose — and the two rows must differ in CF, or a
/// reader that dropped CF entirely would pass.
#[test]
fn adc_after_inc_reads_the_cf_the_block_inherited() {
    for (flags_name, flags) in [("cf clear", LOOP_FLAGS[0]), ("cf set", LOOP_FLAGS[1])] {
        for (esi, _) in LINK_OPERANDS {
            // inc eax · adc esi,esi · setc dl
            let code = [0xFF, 0xC0, 0x11, 0xF6, 0x0F, 0x92, 0xC2];
            let setup = move |r: &mut RegFile| {
                setup_chain(0xffff_ffff, 0, esi, 0)(r);
                r.rflags = Rflags::from(flags);
            };
            let (iced, jit) = simd_dual(&code, &[], setup);
            assert_same_regs(
                &iced,
                &jit,
                &format!("inc preserving {flags_name}, then adc, esi={esi:#x}"),
            );
        }
    }
}

/// `sbb`'s OF is the overflow of `d - s - cf`, **not** of `d - s`.
///
/// `d = 0, s = 0x8000_0000, cf = 1` is the discriminating row: `d - s` overflows
/// (subtracting the most negative value from zero), `d - s - 1 = 0x7fff_ffff`
/// does not. A lowering that derived OF from the plain subtraction — the
/// `PredSource::Plain` form, which is right for `sub` — sets OF for this row and
/// every `jo`/`cmovno` after it goes the wrong way. `seto cl` makes OF a value;
/// RFLAGS at the stop compares the rest of the byte.
#[test]
fn sbb_of_is_the_overflow_of_the_borrow_not_of_the_sub() {
    // `stc` is not lowered, so CF is set the way a guest would have to if it
    // wanted a carry without a writer: shift one out.
    // mov eax,1 · shr eax,1 (CF=1, eax=0) · mov ebx,0x8000_0000 · sbb eax,ebx · seto cl
    let code = [
        0xB8, 0x01, 0x00, 0x00, 0x00, 0xD1, 0xE8, 0xBB, 0x00, 0x00, 0x00, 0x80, 0x19, 0xD8, 0x0F,
        0x90, 0xC1,
    ];
    let setup = |r: &mut RegFile| {
        setup(0, 0)(r);
        r.set_gpr_public(1, 0);
    };
    let (iced, jit) = simd_dual(&code, &[], setup);
    assert_same_regs(
        &iced,
        &jit,
        "sbb32 OF with a carry-in, d=0 s=0x8000_0000 cf=1",
    );

    // The 64-bit form takes the two-half-add path for CF, and its own OF.
    // mov rax,1 · shr rax,1 · mov rbx,0x8000_0000_0000_0000 · sbb rax,rbx · seto cl
    let mut code64 = vec![0x48, 0xB8];
    code64.extend_from_slice(&1_u64.to_le_bytes());
    code64.extend_from_slice(&[0x48, 0xD1, 0xE8, 0x48, 0xBB]);
    code64.extend_from_slice(&0x8000_0000_0000_0000_u64.to_le_bytes());
    code64.extend_from_slice(&[0x48, 0x19, 0xD8, 0x0F, 0x90, 0xC1]);
    let (iced, jit) = simd_dual(&code64, &[], setup);
    assert_same_regs(
        &iced,
        &jit,
        "sbb64 OF with a carry-in, d=0 s=0x8000_0000_0000_0000 cf=1",
    );
}

/// A `pushfq` after a chain observes the packed word, so the deferred `adc`/`sbb`
/// flags have to survive to the boundary that materialises them.
///
/// `add eax,ebx` · `adc ecx,ecx` · `pushfq` · `pop rdi`: the word is the `adc`'s
/// (which defines all six bits), and it is built at `pushfq` from a state that
/// was never packed on the way there.
#[test]
fn pushfq_after_an_adc_chain_reads_the_chains_word() {
    for (case, eax, ebx) in ARITH_CASES {
        for ecx in [0, 0xffff_ffff, 0x7fff_ffff] {
            // add eax,ebx · adc ecx,ecx · pushfq · pop rdi
            let code = [0x01, 0xD8, 0x11, 0xC9, 0x9C, 0x5F];
            let setup = move |r: &mut RegFile| {
                setup(eax, ebx)(r);
                r.set_gpr_public(1, ecx);
                r.set_gpr_public(4, TEST_STACK);
            };
            let (iced, jit) = simd_dual(&code, &[], setup);
            assert_same_regs(
                &iced,
                &jit,
                &format!("pushfq after add->adc, {case}, ecx={ecx:#x}"),
            );
        }
    }
}

/// A **shift between the PF writer and its PF consumer**.
///
/// PF is deferred at the writer (`FlagState::pf_src`: the value whose parity PF
/// is) and the ladder is emitted only where a bit is actually observed. The shift
/// path is the one writer that has to *materialise* the incoming bit — its PF is
/// `select(count_mod == 0, incoming, parity(result))` — and then clear the recipe,
/// because after the select the deferred value is no longer what PF is.
///
/// Nothing else in this file has a shift between a PF writer and a PF reader, and
/// that hole is load-bearing: deleting the `pf_src = None` in `assign_shift`
/// survives every other test in the file, because every other PF consumer reads a
/// recipe that is still correct. Each case is run with both consumers (`setp`,
/// and `pushfq`/`pop rcx`, which packs the whole word), over operand pairs chosen
/// so the pre-shift and post-shift parities disagree — `cmp 1,0` is parity-odd
/// before the shift and parity-even after it.
#[test]
fn a_shift_between_the_pf_writer_and_its_consumer_uses_the_shifted_parity() {
    for (case, eax, ebx) in CMP_CASES {
        // shl eax,1 (D1 E0) / shr eax,1 (D1 E8): both rewrite PF from the result.
        for (shift_name, shift) in [("shl", [0xD1_u8, 0xE0]), ("shr", [0xD1, 0xE8])] {
            // setp cl: FLAGS -> GPR inside the body.
            let mut code = Vec::from(CMP_EAX_EBX);
            code.extend_from_slice(&shift);
            code.extend_from_slice(&setcc_cl(0x0A));
            let (iced, jit) = simd_dual(&code, &[], setup(eax, ebx));
            assert_same_regs(
                &iced,
                &jit,
                &format!("setp after {shift_name} after cmp, {case}"),
            );

            // pushfq; pop rcx: the same bit through the packed word, which is the
            // path a stale recipe would corrupt without touching any GPR.
            let mut code = Vec::from(CMP_EAX_EBX);
            code.extend_from_slice(&shift);
            code.extend_from_slice(&[0x9C, 0x59]);
            let (iced, jit) = simd_dual(&code, &[], setup_with_stack(eax, ebx));
            assert_same_regs(
                &iced,
                &jit,
                &format!("pushfq after {shift_name} after cmp, {case}: the packed word differs"),
            );
        }
    }
}

/// Writers that architecturally **keep** AF, observed through `pushfq`.
///
/// `and`/`or`/`xor`/`test` leave AF exactly as the previous writer set it, the
/// shifts do not touch it, and `mul`/`imul` leave ZF/SF/AF/PF undefined (this
/// lowering keeps them). Only bit 4 of the packed word moves when that rule is
/// broken, which is why the observable is `pushfq`/`pop rcx` and the comparison is
/// the whole register file.
///
/// This row is a **regression test for two separate bugs**, both invisible in
/// every GPR and both found here:
///
/// * `cmp; and; pushfq` handed out `0x246` (AF clear) where the interpreter's
///   word was `0x256` (AF set): the logical writer overwrote the pending `Sub`
///   record — the only place its AF existed — before it was materialised. AF now
///   rides through `PendingFlags::Logic::af_from`.
/// * a pack after a shift or a multiply would lose a *deferred* AF recipe, which
///   is what `assign_shift`/`assign_mul` leaving the recipe alone protects.
///
/// The `and` case additionally pins the whole word against the literal the
/// interpreter produces, so a future change cannot "fix" the divergence by
/// moving the interpreter to the compiled block's answer.
#[test]
fn writers_that_keep_af_do_not_reset_it() {
    let writers: [(&str, &[u8]); 6] = [
        ("and", &[0x23, 0xC3]),        // and eax, ebx
        ("or", &[0x0B, 0xC3]),         // or eax, ebx
        ("xor", &[0x33, 0xC3]),        // xor eax, ebx
        ("test", &[0x85, 0xC3]),       // test eax, ebx
        ("shl", &[0xD1, 0xE0]),        // shl eax, 1
        ("imul", &[0x0F, 0xAF, 0xCA]), // imul ecx, edx
    ];
    for (case, eax, ebx) in CMP_CASES {
        for (name, bytes) in writers {
            let mut code = Vec::from(CMP_EAX_EBX);
            code.extend_from_slice(bytes);
            code.extend_from_slice(&[0x9C, 0x59]); // pushfq; pop rcx
            let (iced, jit) = simd_dual(&code, &[], setup_with_stack(eax, ebx));
            assert_same_regs(
                &iced,
                &jit,
                &format!("pushfq after {name} (AF kept) after cmp, {case}"),
            );
            if name == "and" && eax == 1 && ebx == 2 {
                // The oracle: `cmp 1,2` borrows into bit 4, `and 1,2` keeps it,
                // and the rest of the word is ZF|PF|ALWAYS1|IF.
                assert_eq!(
                    jit.gpr(1),
                    0x256,
                    "and after cmp 1,2: the packed word moved off the interpreter's 0x256"
                );
            }
        }
    }
}
