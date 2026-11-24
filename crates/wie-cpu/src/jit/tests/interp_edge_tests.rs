use super::*;
use crate::IcedCpu;
use crate::exec::degrade_first_sighting;
use iced_x86::Mnemonic;

/// Wave 4 degrade-not-die: an unimplemented mnemonic (x87 `f2xm1`) must NOT
/// stop the interpreter — the fallback advances RIP past the instruction,
/// leaves register state untouched, and counts the execution in the
/// `degraded_insn_count` coverage metric. The counter is a global; the test
/// only asserts a strict increase to stay robust against parallel tests.
#[test]
fn unimplemented_mnemonic_degrades_instead_of_stopping() {
    crate::exec::iced_decode_cache_flush();
    let mut iced = IcedCpu::open_x86_64();
    map_code_region(&mut iced, SIMD_BASE, 0x2000);
    // D9 F0 = f2xm1: an x87 transcendental op with no interpreter
    // implementation (and no x87 register file at all) — the pre-Wave-4
    // behavior was a session stop.
    let code = [0xD9_u8, 0xF0_u8];
    iced.mem_write(SIMD_BASE, &code).expect("iced code");
    iced.write_rip(SIMD_BASE).expect("iced rip");

    let before = crate::degraded_insn_count();
    iced.step_once()
        .expect("degrade fallback must not stop the session");
    let after = crate::degraded_insn_count();
    assert!(
        after > before,
        "the fallback must count the degraded execution (before={before})"
    );
    assert_eq!(
        iced.regs().rip,
        SIMD_BASE + u64::from(code.len() as u32),
        "RIP must advance past the degraded instruction"
    );
}

/// Wave 4 x87 subset: `fld`/`fsub`/`fstp` on m64 operands and the
/// `fcomp` → `fnstsw` condition-code idiom, executed through the iced
/// interpreter (the JIT bails x87 to the interpreter, which now implements
/// the common scalar ops instead of degrading).
#[test]
fn x87_scalar_load_store_arith_and_compare() {
    use crate::regs::X87_C0;

    const CODE: u64 = SIMD_BASE;
    const DATA: u64 = SIMD_BASE + 0x1000;

    crate::exec::iced_decode_cache_flush();
    let mut iced = IcedCpu::open_x86_64();
    map_code_region(&mut iced, SIMD_BASE, 0x2000);

    // Hand-encoded program (RIP-relative displacements filled in below):
    //   DD 05 d1   fld   qword [rip+a]    ; push 6.0
    //   DC 25 d2   fsub  qword [rip+b]    ; st0 = 6.0 - 4.0
    //   DD 1D d3   fstp  qword [rip+z]    ; z = 2.0, pop
    //   DF E0      fnstsw ax              ; sanity: TOP back at 0
    let code: Vec<u8> = {
        let mut c: Vec<u8> = Vec::new();
        c.extend_from_slice(&[0xDD, 0x05]);
        c.extend_from_slice(
            &(DATA.wrapping_sub(CODE + 6) as u32).to_le_bytes(), // a
        );
        let s2 = u64::try_from(c.len()).unwrap_or(0);
        c.extend_from_slice(&[0xDC, 0x25]);
        c.extend_from_slice(
            &(DATA.wrapping_add(8).wrapping_sub(CODE + s2 + 6) as u32).to_le_bytes(), // b
        );
        let s3 = u64::try_from(c.len()).unwrap_or(0);
        c.extend_from_slice(&[0xDD, 0x1D]);
        c.extend_from_slice(
            &(DATA.wrapping_add(16).wrapping_sub(CODE + s3 + 6) as u32).to_le_bytes(), // z
        );
        c.extend_from_slice(&[0xDF, 0xE0]);
        c
    };
    let bytes = |v: f64| v.to_bits().to_le_bytes();
    iced.mem_write(CODE, &code).expect("code");
    iced.mem_write(DATA, &bytes(6.0)).expect("a");
    iced.mem_write(DATA + 8, &bytes(4.0)).expect("b");
    iced.mem_write(DATA + 16, &bytes(0.0)).expect("z");
    iced.write_rip(CODE).expect("rip");

    for _ in 0..4 {
        iced.step_once().expect("x87 step");
    }
    let mut out = [0_u8; 8];
    iced.mem_read(DATA + 16, &mut out).expect("read z");
    assert_eq!(
        f64::from_bits(u64::from_le_bytes(out)),
        2.0,
        "fld/fsub/fstp must compute 6.0 - 4.0"
    );
    assert_eq!(
        iced.regs().x87_top,
        0,
        "the stack must be back at TOP=0 after push/sub/pop"
    );

    // fcomp → fnstsw: 4.0 < 6.0 must set C0 (the sahf/jb idiom).
    let code2: Vec<u8> = {
        let base = CODE + 0x100;
        let mut c: Vec<u8> = Vec::new();
        c.extend_from_slice(&[0xDD, 0x05]);
        c.extend_from_slice(&(DATA.wrapping_add(8).wrapping_sub(base + 6) as u32).to_le_bytes()); // fld b
        let s2 = u64::try_from(c.len()).unwrap_or(0);
        c.extend_from_slice(&[0xDC, 0x1D]);
        c.extend_from_slice(
            &(DATA.wrapping_sub(base + s2 + 6) as u32).to_le_bytes(), // fcomp a
        );
        c.extend_from_slice(&[0xDF, 0xE0]); // fnstsw ax
        c
    };
    iced.mem_write(CODE + 0x100, &code2).expect("code2");
    iced.write_rip(CODE + 0x100).expect("rip2");
    for _ in 0..3 {
        iced.step_once().expect("x87 compare step");
    }
    let ax = iced.regs().read_reg(iced_x86::Register::AX).unwrap_or(0);
    assert_ne!(
        ax & u64::from(X87_C0),
        0,
        "fcomp(4.0, 6.0) must set C0 (st0 < other)"
    );
}

/// The "trace once per mnemonic" latch must let exactly ONE caller through per
/// mnemonic, whatever the interleaving.
///
/// This is the property `degrade_fallback`'s log depends on, and it used to
/// come from `HashSet::insert`'s boolean under a mutex. It now comes from
/// `AtomicU64::fetch_or` returning the previous word — same guarantee, no lock
/// on a per-instruction path. `fdecstp`/`fincstp` are used by nothing else in
/// the tree, so the process-global bits these assertions consume cannot be
/// consumed by a parallel test.
#[test]
fn degrade_trace_latch_admits_exactly_once_per_mnemonic() {
    assert!(
        degrade_first_sighting(Mnemonic::Fdecstp),
        "first sighting must win the latch"
    );
    assert!(
        !degrade_first_sighting(Mnemonic::Fdecstp),
        "second sighting of the same mnemonic must not log again"
    );
    assert!(
        !degrade_first_sighting(Mnemonic::Fdecstp),
        "the latch stays closed"
    );
}

/// Same latch, raced: eight threads call it for one fresh mnemonic and exactly
/// one of them may observe the bit clear. `fetch_or` is what makes that hold —
/// a load-then-store would admit several winners.
#[test]
fn degrade_trace_latch_has_one_winner_under_contention() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    let winners = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(8));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let winners = Arc::clone(&winners);
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            if degrade_first_sighting(Mnemonic::Fincstp) {
                winners.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    for h in handles {
        drop(h.join());
    }
    assert_eq!(
        winners.load(Ordering::Relaxed),
        1,
        "exactly one thread may emit the warn line for a mnemonic"
    );
}
