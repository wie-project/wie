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
/// the comparison covers the branch as well as RFLAGS.
#[test]
fn jcc_reads_flags_set_earlier_in_the_block() {
    for (case, eax, ebx) in CMP_CASES {
        for (cc, name) in COND_NAMES.iter().enumerate() {
            let cc = u8::try_from(cc).expect("0..16");
            let mut code = Vec::from(CMP_EAX_EBX);
            code.extend_from_slice(&[0x70 | cc, 0x01, 0x90, 0x0F, 0x0B]);
            // Stop on the first RIP at or past the `nop`: reached directly when
            // the branch is not taken, and skipped over when it is.
            let stop = super::SIMD_BASE + 6;
            let (iced, jit) = chain_dual(&code, stop, setup(eax, ebx), |_| {});
            assert_same_regs(&iced, &jit, &format!("j{name} after cmp, {case}"));
            assert!(
                iced.rip == super::SIMD_BASE + 6 || iced.rip == super::SIMD_BASE + 7,
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
