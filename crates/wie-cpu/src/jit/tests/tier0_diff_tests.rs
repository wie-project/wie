//! The tier-0 differential oracle: **the gate the emitter must pass**, built
//! before the emitter exists.
//!
//! # What it compares, and why against Cranelift
//!
//! The emitter replaces one thing — what a guest block lowers to — so the
//! reference implementation is the Cranelift block it replaces, not the
//! interpreter. For the same guest bytes and the same initial state, the two
//! implementations must leave the same **guest-observable** state: the 16 GPRs,
//! the RFLAGS word and the exit RIP. Those three are what a block can change that
//! the guest can see; everything else the dispatcher reads (retired count, hop
//! counters) is accounting, not semantics. The fault word is deliberately **not**
//! an observable yet: no fixture here can fault, so comparing it would be a field
//! that can never differ — it joins the set with the memory-operand class, which
//! is the first one where a fault is reachable.
//!
//! # The oracle must be able to fail
//!
//! A harness that only ever compares Cranelift with itself proves nothing, and
//! this campaign has already shipped a self-agreeing diagnostic once (the tier-0
//! classifier agreed with its own blocker counter while both were wrong). So the
//! test set includes **deliberately wrong blocks** — installed through the same
//! `insert_ready` + `step_one` path a real tier-0 block will use, with the same
//! `unsafe extern "C" fn(*mut JitCtx)` ABI, so the harness cannot tell them apart
//! from emitted code by construction:
//!
//! - [`mutant_drops_the_flag_write`] — arithmetic right, flags stale. Must be
//!   caught in `rflags`.
//! - [`mutant_zero_extends_where_it_must_preserve`] — value right in the low
//!   half, upper bits clobbered. Must be caught in `gpr`, not in `rflags`.
//!
//! Both must be caught *and named*, and [`the_control_arm_agrees_with_itself`]
//! must be clean, or a harness that fails everything would look like a success.

#![allow(clippy::expect_used)]

use super::harness::{map_code_page, map_code_region};
use crate::CpuEngine;
use crate::jit::JitCpu;
use crate::jit::lower::{CompiledBlock, JitCtx};
use crate::jit::tier::OptTier;
use crate::regs::Rflags;

/// Guest code used by every case: 32-bit `sub eax, ebx`, so both a GPR and the
/// arithmetic flags change, and a `jmp` so the block has a terminator and an exit
/// RIP distinct from its entry.
const FIXTURE: [u8; 4] = [
    0x29, 0xD8, // sub eax, ebx
    0xEB, 0x00, // jmp $+2
];
const ENTRY: u64 = 0x10c0_0000;
const EXIT: u64 = 0x10c0_0004;
/// 16-bit `add ax, bx`, the shape whose high bits must survive.
const WIDE_FIXTURE: [u8; 5] = [
    0x66, 0x01, 0xD8, // add ax, bx
    0xEB, 0x00, // jmp $+2
];

/// Everything a guest can observe about one block execution.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Observables {
    gpr: [u64; 16],
    rflags: u64,
    rip: u64,
}

impl Observables {
    fn of(cpu: &JitCpu) -> Self {
        let mut gpr = [0_u64; 16];
        for (i, slot) in gpr.iter_mut().enumerate() {
            *slot = cpu.thread.regs.gpr(i);
        }
        Self {
            gpr,
            rflags: u64::from(cpu.thread.regs.rflags),
            rip: cpu.thread.regs.rip,
        }
    }

    /// Field-level differences, named, so a failure says *which* observable
    /// diverged rather than dumping two 16-word vectors.
    fn diff(&self, other: &Self) -> Vec<String> {
        let mut out = Vec::new();
        const NAMES: [&str; 16] = [
            "RAX", "RCX", "RDX", "RBX", "RSP", "RBP", "RSI", "RDI", "R8", "R9", "R10", "R11",
            "R12", "R13", "R14", "R15",
        ];
        for (i, (a, b)) in self.gpr.iter().zip(other.gpr.iter()).enumerate() {
            if a != b {
                out.push(format!("{}: {a:#x} vs {b:#x}", NAMES[i]));
            }
        }
        if self.rflags != other.rflags {
            out.push(format!("rflags: {:#x} vs {:#x}", self.rflags, other.rflags));
        }
        if self.rip != other.rip {
            out.push(format!("rip: {:#x} vs {:#x}", self.rip, other.rip));
        }
        out
    }
}

/// Which implementation of the block runs.
enum Arm {
    /// The reference: the Cranelift translation of the fixture bytes.
    Reference,
    /// A hand-written block installed as `Ready`, exercising the same ABI a
    /// tier-0 block will. Deliberately may be *wrong* — that is the point.
    HandWritten {
        func: unsafe extern "C" fn(*mut JitCtx),
        insn_count: u32,
    },
}

/// Build an engine with `bytes` at `base`, install `arm` for `ENTRY`, run one
/// `step_one`, and snapshot the guest-visible state.
fn run(base: u64, bytes: &[u8], rax: u64, rbx: u64, arm: Arm) -> Observables {
    let mut cpu = Box::new(JitCpu::open_x86_64());
    map_code_page(cpu.as_mut(), base);
    cpu.mem_write(base, bytes).expect("write fixture bytes");
    cpu.thread.regs.set_gpr(0, rax);
    cpu.thread.regs.set_gpr(3, rbx);
    cpu.thread.regs.rflags = Rflags::DEFAULT;
    cpu.write_rip(base).expect("set rip");

    let compiled = match arm {
        Arm::Reference => cpu
            .try_compile(base)
            .expect("Cranelift compiles the fixture"),
        Arm::HandWritten { func, insn_count } => CompiledBlock {
            func,
            func_id: None,
            tier: OptTier::Base,
            insn_count,
            guest_start: base,
            guest_end: base + u64::from(u32::try_from(bytes.len()).expect("fits")),
            inv_gen: 0,
        },
    };
    cpu.insert_ready(base, compiled);
    cpu.step_one().expect("the block must run");
    Observables::of(&cpu)
}

/// The bytes the reference arm compiles. `run` writes them into guest memory, so
/// the mutants can be paired with the same fixture.
fn fixture() -> Observables {
    run(ENTRY, &FIXTURE, 0x10, 0x03, Arm::Reference)
}

// ---- the mutants ---------------------------------------------------------
//
// Both are `extern "C" fn(*mut JitCtx)` — the same signature Cranelift emits —
// and both are installed through `insert_ready` and reached through
// `step_one()`, which is why the harness cannot distinguish them from a real
// tier-0 block. That is what makes them a proof of the oracle rather than a
// unit test of the mutants.

/// Arithmetic correct, **flags never written**: the classic "the emitter forgot
/// `subs` so the following `jne` takes the other side" bug.
unsafe extern "C" fn mutant_drops_the_flag_write(ctx: *mut JitCtx) {
    // SAFETY: the harness passes a live `JitCtx` for the duration of the call.
    let ctx = unsafe { &mut *ctx };
    let rax = ctx.gpr[0] & 0xFFFF_FFFF;
    let rbx = ctx.gpr[3] & 0xFFFF_FFFF;
    ctx.gpr[0] = rax.wrapping_sub(rbx) & 0xFFFF_FFFF;
    // rflags deliberately untouched.
    ctx.rip = EXIT;
}

/// Value correct in the low half, **upper bits clobbered**: the `add ax, bx`
/// shape implemented as if it were 32-bit, so the zero-extension case is applied
/// where preservation is required.
unsafe extern "C" fn mutant_zero_extends_where_it_must_preserve(ctx: *mut JitCtx) {
    // SAFETY: as above.
    let ctx = unsafe { &mut *ctx };
    let ax = (ctx.gpr[0] & 0xFFFF).wrapping_add(ctx.gpr[3] & 0xFFFF) & 0xFFFF;
    ctx.gpr[0] = ax; // must be `(old & !0xFFFF) | ax`
    ctx.rflags = u64::from(Rflags::DEFAULT);
    ctx.rip = EXIT;
}

#[test]
fn the_control_arm_agrees_with_itself() {
    // Green by construction: same bytes, same arm, twice. If this ever fails the
    // oracle is measuring something that is not deterministic (a timestamp, an
    // address, an RNG) and every other assertion below is worthless.
    let a = fixture();
    let b = fixture();
    assert_eq!(a, b);
    assert!(a.diff(&b).is_empty());
    // Sanity on the reference itself: `sub eax, ebx` must actually write both a
    // GPR and the flags, or the mutant tests below could pass vacuously.
    assert_ne!(a.gpr[0], 0x10, "the fixture must change RAX");
    assert_ne!(a.rflags, u64::from(Rflags::DEFAULT), "and must write flags");
    assert_eq!(a.rip, EXIT);
}

#[test]
fn the_oracle_catches_a_dropped_flag_write() {
    let good = fixture();
    let bad = run(
        ENTRY,
        &FIXTURE,
        0x10,
        0x03,
        Arm::HandWritten {
            func: mutant_drops_the_flag_write,
            insn_count: 2,
        },
    );
    let diff = good.diff(&bad);
    assert!(!diff.is_empty(), "a dropped flag write must be caught");
    assert!(
        diff.iter().any(|d| d.starts_with("rflags:")),
        "it must be named as a flags divergence, got {diff:?}"
    );
    // And the value must still be right, so the test is proving it caught the
    // *flag* bug specifically rather than "different".
    assert_eq!(
        bad.gpr[0], good.gpr[0],
        "the mutant's arithmetic is correct"
    );
}

#[test]
fn the_oracle_catches_a_zero_extension_where_preservation_is_required() {
    let good = run(
        ENTRY,
        &WIDE_FIXTURE,
        0xFFFF_FFFF_FFFF_0001,
        0,
        Arm::Reference,
    );
    let bad = run(
        ENTRY,
        &WIDE_FIXTURE,
        0xFFFF_FFFF_FFFF_0001,
        0,
        Arm::HandWritten {
            func: mutant_zero_extends_where_it_must_preserve,
            insn_count: 2,
        },
    );
    assert_eq!(
        good.gpr[0], 0xFFFF_FFFF_FFFF_0001,
        "precondition: `add ax, 0` must preserve the upper 48 bits"
    );
    let diff = good.diff(&bad);
    assert!(!diff.is_empty(), "a clobbered upper half must be caught");
    assert!(
        diff.iter().any(|d| d.starts_with("RAX:")),
        "it must be named as a RAX divergence, got {diff:?}"
    );
}

/// The number the emitter has to beat: **µs per block for the Cranelift
/// translation of the same fixture**, measured in this process so it is
/// comparable to the emitter's own measurement later.
///
/// `#[ignore]` because it is a timing measurement: it belongs in an explicit
/// `--release` run, not in the suite where a loaded host would make it flaky.
/// It asserts nothing about the *result* for that reason — the assertion is that
/// the measurement ran, and the number is reported by the harness:
///
/// ```text
/// cargo nextest run --release -p wie-cpu -E 'test(reference_compile_cost)' --run-ignored all
/// ```
#[test]
#[ignore = "timing measurement: run explicitly in release"]
fn reference_compile_cost_per_block() {
    const N: u32 = 100;
    /// Blocks compiled before timing, purely to grow the arena.
    const GROW: u32 = 200;
    let mut cpu = Box::new(JitCpu::open_x86_64());
    // One page for every block: `map_code_page` per block would reserve the same
    // page N times and fail with "reserve over non-free page".
    map_code_region(cpu.as_mut(), ENTRY, 8192);
    // Distinct RIPs at 8-byte strides so nothing is memoised and no two blocks
    // overlap: the emitter's cost must be measured on fresh blocks.
    for i in 0..GROW + N {
        let at = ENTRY + u64::from(i) * 8;
        cpu.mem_write(at, &FIXTURE).expect("fixture bytes");
    }
    // Pre-grow the module arena: the JIT's code buffer is `mmap`ed and
    // re-`mprotect`ed as it grows, so early compiles pay for the *arena*, not
    // for the block. Measuring without this reported 15 ms/block, which is the
    // growth cost and not the number the emitter has to beat.
    for i in 0..GROW {
        let at = ENTRY + u64::from(i) * 8;
        let _ = cpu.try_compile(at).expect("grow");
    }

    let start = std::time::Instant::now();
    for i in GROW..GROW + N {
        let at = ENTRY + u64::from(i) * 8;
        let _ = cpu.try_compile(at).expect("compile");
    }
    let per_block_us = start.elapsed().as_secs_f64() * 1e6 / f64::from(N);
    eprintln!(
        "tier0 reference: Cranelift {per_block_us:.1} us/block over {N} distinct blocks \
         (release, in-process, same fixture)"
    );
    assert!(per_block_us > 0.0, "the measurement must produce a number");
}
