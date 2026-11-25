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
//! counters) is accounting, not semantics.
//!
//! The observable set grew with the class it covers. For code-only fixtures the
//! fault word was deliberately excluded, because a field that can never differ
//! proves nothing — it joined the set with the memory-operand fixtures, which are
//! the first where a fault is reachable, and [`mutant_swallows_the_fault`] exists
//! so the field is known to be able to fail. The same reasoning put the **watched
//! data window** into every observation: a store that lands at the wrong address
//! is invisible to a register-only comparison, and
//! [`mutant_stores_through_the_wrong_address`] is there to prove the window
//! catches it.
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
//! - [`mutant_drops_the_displacement`] — reads `[rbx]` where the block says
//!   `[rbx+8]`. Must be caught in `gpr`.
//! - [`mutant_stores_through_the_wrong_address`] — writes `[rbx]` where the
//!   block says `[rbx+8]`. Must be caught in the watched **data window** and in
//!   no register: this is the bug a register-only oracle cannot see.
//! - [`mutant_swallows_the_fault`] — an unmapped load treated as a readable
//!   zero. Must be caught in the **fault word**: nothing else differs.
//!
//! Both must be caught *and named*, and [`the_control_arm_agrees_with_itself`]
//! must be clean, or a harness that fails everything would look like a success.

#![allow(clippy::expect_used)]

use super::harness::{map_code_page, map_code_region};
use crate::CpuEngine;
use crate::exec::{AccessType, StepResult};
use crate::jit::JitCpu;
use crate::jit::lower::{CompiledBlock, JitCtx};
use crate::jit::tier::OptTier;
use crate::regs::Rflags;

/// Guest code used by every case: 32-bit `sub eax, ebx`, so both a GPR and the
/// arithmetic flags change, then a `jmp` over a `nop` so the block ends on a real
/// terminator.
///
/// **The tail is `jmp +1`, not `jmp +0`, and that detail cost this harness its
/// first version.** `decode_pure_gpr_block` deliberately merges a `jmp` whose
/// target *is* the next instruction — it is a no-op jump, so the decoder treats it
/// as fall-through and keeps decoding. A `jmp $+2` fixture therefore has no
/// terminator at all: the block ran on through the zero-filled page for up to
/// `MAX_BLOCK_INSNS`, executed `add [rax], al` against whatever the fixture had
/// left in RAX, and reported a fault. The oracle agreed with itself the whole
/// time — both arms ran the same 95-instruction block — so every case passed
/// while measuring something other than the fixture. Jumping *over* a byte makes
/// the target differ from the next IP, which is what makes it a terminator.
///
/// [`retired`](Observables::retired) is asserted against the expected instruction
/// count everywhere for exactly this reason.
const FIXTURE: [u8; 5] = [
    0x29, 0xD8, // sub eax, ebx
    0xEB, 0x01, // jmp +1 (over the nop)
    0x90, // nop — skipped, never executed
];
const ENTRY: u64 = 0x10c0_0000;
const EXIT: u64 = 0x10c0_0005;
/// Instructions each fixture block is. Asserted against `retired`, so a fixture
/// that silently decodes into the page tail cannot pass.
const FIXTURE_INSNS: usize = 2;
/// 16-bit `add ax, bx`, the shape whose high bits must survive.
const WIDE_FIXTURE: [u8; 6] = [
    0x66, 0x01, 0xD8, // add ax, bx
    0xEB, 0x01, // jmp +1
    0x90, // nop
];
const WIDE_EXIT: u64 = 0x10c0_0006;

/// Everything a guest can observe about one block execution.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Observables {
    gpr: [u64; 16],
    rflags: u64,
    rip: u64,
    /// The fault word: `None` for a clean block, otherwise what the guest is
    /// entitled to see about the faulting access.
    ///
    /// `JitCtx` carries four fields for this (`fault`, `fault_addr`,
    /// `fault_size`, `fault_access`); the dispatcher folds the first into
    /// `Some(..)`/`None` and translates the rest into [`exec::InvalidMem`], which
    /// is what the runtime acts on. The oracle compares the dispatcher's view,
    /// because that is the contract an emitter has to satisfy — the raw `JitCtx`
    /// layout is a private detail two emitters could legitimately disagree about.
    fault: Option<FaultWord>,
    /// The watched data window ([`Data`]), so a store to the wrong address is
    /// observable rather than invisible.
    data: Vec<u8>,
    /// Guest instructions the step charged. **Not** compared by [`Self::diff`]:
    /// it is dispatcher accounting, not semantics, and an emitter is entitled to
    /// charge differently.
    ///
    /// It is here as a **fixture-shape guard**, and it exists because this harness
    /// lost a round to not having it. The first version of every fixture ended in
    /// `jmp $+2`, which `decode_pure_gpr_block` merges as a no-op jump (target ==
    /// next IP), so the block had no terminator: it decoded on through the
    /// zero-filled page and ran 95 instructions — `add [rax], al` against whatever
    /// the fixture had left in RAX — before faulting. Both arms ran the *same*
    /// wrong block, so the oracle stayed perfectly self-consistent while measuring
    /// something that was not the fixture (`retired` 95, fault word
    /// `Some(Read, <whatever RAX held>, 1)`) and every case passed.
    ///
    /// Every case that runs the reference therefore asserts
    /// `retired == FIXTURE_INSNS`: a fixture that silently grows a tail fails
    /// there instead of quietly changing what the rest of the test means.
    retired: usize,
}

/// What the guest sees about a faulting access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FaultWord {
    access: &'static str,
    address: u64,
    size: i32,
}

impl Observables {
    fn of(cpu: &mut JitCpu, result: &StepResult, retired: usize) -> Self {
        let mut gpr = [0_u64; 16];
        for (i, slot) in gpr.iter_mut().enumerate() {
            *slot = cpu.thread.regs.gpr(i);
        }
        let fault = match result {
            StepResult::InvalidMemory(m) => Some(FaultWord {
                access: match m.access_type {
                    AccessType::Read => "read",
                    AccessType::Write => "write",
                    AccessType::Fetch => "fetch",
                },
                address: m.address,
                size: m.size,
            }),
            StepResult::Continue | StepResult::HostStop { .. } => None,
        };
        let mut data = vec![0_u8; Data::LEN];
        let read = cpu.mem_read(Data::BASE, &mut data);
        assert!(
            read.is_ok(),
            "the watched data window must be mapped: {read:?}"
        );
        Self {
            gpr,
            rflags: u64::from(cpu.thread.regs.rflags),
            rip: cpu.thread.regs.rip,
            fault,
            data,
            retired,
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
        if self.fault != other.fault {
            out.push(format!("fault: {:?} vs {:?}", self.fault, other.fault));
        }
        for (i, (a, b)) in self.data.iter().zip(other.data.iter()).enumerate() {
            if a != b {
                out.push(format!("data[+{i}]: {a:#04x} vs {b:#04x}"));
            }
        }
        out
    }
}

/// The mapped data window every observation compares.
///
/// A page of its own, ending one page below [`Data::UNMAPPED`]: the legal cases
/// point into it, the faulting cases point one past its end. It is small on
/// purpose — an address mistake that drops a displacement lands *inside* the
/// window, which is the mistake worth catching.
struct Data;
impl Data {
    const BASE: u64 = 0x20c0_0000;
    const LEN: usize = 32;
    /// Address nothing maps, so an access there faults.
    ///
    /// **Sixteen mebibytes away, not one page away, and that distance is a bug
    /// fix.** Guest allocations round to the 64 KiB Windows allocation
    /// granularity, so `BASE + 0x1000` sits *inside* the same reservation and
    /// reads fine: the first version of this constant was one page up, both
    /// "faulting" cases silently succeeded, and the two tests that exist to make
    /// the fault word observable were asserting against a field that could never
    /// carry it. Keep the offset large enough that no plausible reservation
    /// rounding can swallow it — a fault fixture that cannot fault is worse than
    /// no fault fixture, because it makes the fault word look covered.
    const UNMAPPED: u64 = 0x2100_0000;
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

/// The data window as every case starts it.
///
/// Two different words at two different offsets, so an address mistake reads a
/// *different* value rather than the same one at the wrong place.
fn data_window() -> [u8; Data::LEN] {
    let mut d = [0_u8; Data::LEN];
    d[0..4].copy_from_slice(&0x99_u32.to_le_bytes());
    d[8..12].copy_from_slice(&0x2a_u32.to_le_bytes());
    d
}

/// Build an engine with `bytes` at `base`, install `arm` for `ENTRY`, run one
/// `step_one`, and snapshot the guest-visible state.
fn run(base: u64, bytes: &[u8], rax: u64, rbx: u64, arm: Arm) -> Observables {
    run_mem(base, bytes, rax, rbx, data_window(), arm)
}

/// As [`run`], with the initial contents of the watched data window.
fn run_mem(
    base: u64,
    bytes: &[u8],
    rax: u64,
    rbx: u64,
    data: [u8; Data::LEN],
    arm: Arm,
) -> Observables {
    let mut cpu = Box::new(JitCpu::open_x86_64());
    map_code_page(cpu.as_mut(), base);
    // The data page the memory-operand fixtures point at. Mapping it in every
    // case — including the code-only ones — keeps the observation identical
    // across fixtures, so a mutant cannot pass by writing into unmapped space.
    map_code_region(cpu.as_mut(), Data::BASE, 0x1000);
    cpu.mem_write(base, bytes).expect("write fixture bytes");
    cpu.mem_write(Data::BASE, &data).expect("write data window");
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
    let (result, retired) = cpu.step_one().expect("the block must run");
    Observables::of(&mut cpu, &result, retired)
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
    ctx.rip = WIDE_EXIT;
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
    // The regression guard for this harness's own first version: a `jmp $+2` tail
    // is merged as a no-op, so the block ran 95 instructions into the zero page,
    // faulted there, and both arms agreed — a self-consistent oracle measuring the
    // wrong block. See `Observables::retired`.
    assert_eq!(
        a.retired, FIXTURE_INSNS,
        "the fixture must be the {FIXTURE_INSNS} instructions it claims, not a \
         block that decoded on into the page tail"
    );
    assert!(
        a.fault.is_none(),
        "the control fixture executes nothing invalid: a fault here means the \
         block ran past its terminator into the page tail"
    );
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
    assert_eq!(good.retired, FIXTURE_INSNS, "two instructions, no tail");
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

// ---- the memory-operand class -------------------------------------------
//
// The first class where a fault is reachable, which is why the fault word and
// the watched data window join the observable set here and not before.
//
// `mov eax, [rbx+8]` / `mov [rbx+8], eax`, then `jmp $+2`: a displacement is part
// of the address, so "the address calculation is wrong" is a real bug with a real
// wrong answer (the window holds a *different* word at +0 than at +8), and the
// same two encodings serve the faulting cases by pointing `rbx` one page higher,
// where nothing is mapped.

/// `mov eax, [rbx+8]; jmp +1; nop`
const LOAD_DISP8: [u8; 6] = [0x8B, 0x43, 0x08, 0xEB, 0x01, 0x90];
/// `mov [rbx+8], eax; jmp +1; nop`
const STORE_DISP8: [u8; 6] = [0x89, 0x43, 0x08, 0xEB, 0x01, 0x90];
/// Exit RIP of both: the `jmp` skips the `nop`, so control resumes past it.
const MEM_EXIT: u64 = ENTRY + 6;

#[test]
fn a_legal_load_reads_through_the_displaced_address() {
    let got = run(ENTRY, &LOAD_DISP8, 0, Data::BASE, Arm::Reference);
    assert_eq!(got.retired, FIXTURE_INSNS, "two instructions, no tail");
    assert_eq!(got.gpr[0], 0x2a, "RAX must hold the word at [rbx+8]");
    assert_eq!(got.fault, None, "a legal load must not report a fault");
    assert_eq!(got.rip, MEM_EXIT);
    assert_eq!(
        got.data,
        data_window(),
        "a load must not modify guest memory"
    );
}

#[test]
fn a_legal_store_writes_through_the_displaced_address() {
    let got = run(ENTRY, &STORE_DISP8, 0x2a, Data::BASE, Arm::Reference);
    assert_eq!(got.retired, FIXTURE_INSNS, "two instructions, no tail");
    assert_eq!(got.fault, None, "a legal store must not report a fault");
    assert_eq!(
        &got.data[8..12],
        &0x2a_u32.to_le_bytes(),
        "stored at [rbx+8]"
    );
    assert_eq!(
        &got.data[0..4],
        &0x99_u32.to_le_bytes(),
        "the word at [rbx] must be untouched: the displacement is part of the address"
    );
    assert_eq!(got.rip, MEM_EXIT);
}

#[test]
fn a_faulting_load_reports_the_address_and_a_read() {
    let got = run(ENTRY, &LOAD_DISP8, 0, Data::UNMAPPED - 8, Arm::Reference);
    assert_eq!(
        got.fault,
        Some(FaultWord {
            access: "read",
            address: Data::UNMAPPED,
            size: 4
        }),
        "an unmapped load must report [rbx+8] as a 4-byte read fault"
    );
    assert_eq!(got.rip, ENTRY, "the faulting IP is the block's own IP");
    assert_eq!(
        got.data,
        data_window(),
        "a faulting access must not modify memory"
    );
}

#[test]
fn a_faulting_store_reports_the_address_and_a_write() {
    let got = run(
        ENTRY,
        &STORE_DISP8,
        0x2a,
        Data::UNMAPPED - 8,
        Arm::Reference,
    );
    assert_eq!(
        got.fault,
        Some(FaultWord {
            access: "write",
            address: Data::UNMAPPED,
            size: 4
        }),
        "an unmapped store must report [rbx+8] as a 4-byte write fault"
    );
    assert_eq!(got.rip, ENTRY);
    assert_eq!(
        got.data,
        data_window(),
        "a faulting store must not modify memory"
    );
}

/// **Address calculation wrong on a legal load**: reads `[rbx]` where the block
/// says `[rbx+8]`, through the real guest memory, so the value it produces is
/// the correct value of the *wrong* address — exactly what a mis-emitted
/// displacement does in production.
unsafe extern "C" fn mutant_drops_the_displacement(ctx: *mut JitCtx) {
    // SAFETY: the harness passes a live `JitCtx` with a live `mem` for the
    // duration of the call, exactly as it does for emitted code.
    let ctx = unsafe { &mut *ctx };
    let addr = ctx.gpr[3];
    let mut buf = [0_u8; 4];
    let ok = unsafe { &*ctx.mem }.read(addr, &mut buf).is_ok();
    ctx.gpr[0] = if ok {
        u64::from(u32::from_le_bytes(buf))
    } else {
        0
    };
    ctx.rip = MEM_EXIT;
}

/// **Address calculation wrong on a legal store**: writes `[rbx]` where the block
/// says `[rbx+8]`. Invisible to every register, which is why the data window had
/// to join the observable set.
unsafe extern "C" fn mutant_stores_through_the_wrong_address(ctx: *mut JitCtx) {
    // SAFETY: as above.
    let ctx = unsafe { &mut *ctx };
    let addr = ctx.gpr[3];
    let value = u32::try_from(ctx.gpr[0] & 0xFFFF_FFFF).unwrap_or(0);
    // The write result is deliberately ignored: the point is where it lands.
    drop(unsafe { &mut *ctx.mem }.write(addr, &value.to_le_bytes()));
    ctx.rip = MEM_EXIT;
}

/// **Fault swallowed**: an unmapped load treated as a readable zero.
///
/// This is the mutant that justifies the fault word being an observable: a block
/// that faults and a block that invents a value agree on every GPR the guest
/// would otherwise look at, and only the fault distinguishes them.
unsafe extern "C" fn mutant_swallows_the_fault(ctx: *mut JitCtx) {
    // SAFETY: as above.
    let ctx = unsafe { &mut *ctx };
    ctx.gpr[0] = 0; // "the read returned zero"
    ctx.fault = 0;
    ctx.rip = MEM_EXIT;
}

#[test]
fn the_oracle_catches_a_dropped_displacement() {
    let good = run(ENTRY, &LOAD_DISP8, 0, Data::BASE, Arm::Reference);
    let bad = run(
        ENTRY,
        &LOAD_DISP8,
        0,
        Data::BASE,
        Arm::HandWritten {
            func: mutant_drops_the_displacement,
            insn_count: 2,
        },
    );
    assert_eq!(
        good.gpr[0], 0x2a,
        "precondition: the reference reads [rbx+8]"
    );
    let diff = good.diff(&bad);
    assert!(!diff.is_empty(), "a dropped displacement must be caught");
    assert!(
        diff.iter().any(|d| d.starts_with("RAX:")),
        "it must be named as a RAX divergence, got {diff:?}"
    );
    assert_eq!(bad.gpr[0], 0x99, "the mutant read the word at [rbx]");
    assert_eq!(bad.fault, None, "and it did not fault: this is a value bug");
}

#[test]
fn the_oracle_catches_a_store_through_the_wrong_address() {
    let good = run(ENTRY, &STORE_DISP8, 0x2a, Data::BASE, Arm::Reference);
    let bad = run(
        ENTRY,
        &STORE_DISP8,
        0x2a,
        Data::BASE,
        Arm::HandWritten {
            func: mutant_stores_through_the_wrong_address,
            insn_count: 2,
        },
    );
    let diff = good.diff(&bad);
    assert!(!diff.is_empty(), "a mis-addressed store must be caught");
    assert!(
        diff.iter().any(|d| d.starts_with("data[+0]")),
        "it must be named against the watched data window, got {diff:?}"
    );
    assert!(
        diff.iter().all(|d| !d.starts_with("RAX:")),
        "and no register may diverge: the store bug is only visible in memory, got {diff:?}"
    );
}

#[test]
fn the_oracle_catches_a_swallowed_fault() {
    let good = run(ENTRY, &LOAD_DISP8, 0, Data::UNMAPPED - 8, Arm::Reference);
    let bad = run(
        ENTRY,
        &LOAD_DISP8,
        0,
        Data::UNMAPPED - 8,
        Arm::HandWritten {
            func: mutant_swallows_the_fault,
            insn_count: 2,
        },
    );
    assert!(good.fault.is_some(), "precondition: the reference faults");
    let diff = good.diff(&bad);
    assert!(!diff.is_empty(), "a swallowed fault must be caught");
    assert!(
        diff.iter().any(|d| d.starts_with("fault:")),
        "it must be named in the fault word, got {diff:?}"
    );
}

/// The number the emitter has to beat: **Cranelift µs per block on the real
/// block set** — the `with_mem` blocks inside a real guest image's `.text`.
///
/// `#[ignore]` because it is a measurement, not a gate: it reads `real_exes/`
/// (not shipped to CI) and compiles thousands of functions. It asserts only that
/// a number came out, never what the number is.
///
/// This exists because the cost side was the missing half of the tier-0
/// decision: `census` told us how many blocks a single-pass emitter could take
/// (1 083 `alu_only` + 690 `with_mem`), and nothing had ever measured what the
/// Cranelift block it would replace actually costs. Without that, "0.1-0.3 ms
/// per block" is a target with no denominator.
///
/// ```text
/// cargo test --release -p wie-cpu --lib \
///   jit::tests::tier0_diff_tests::reference_compile_cost_per_block -- --ignored --nocapture
/// ```
///
/// The block set is a **linear sweep** of `.text`: every block the decoder finds
/// starting at an instruction boundary of the previous block, which is what a
/// first-sight walk of a real image looks like (a real run additionally biases
/// towards branch targets). Blocks are decoded once, outside the timed region, so
/// the number is the *compile*, not the iced decode in front of it.
#[test]
#[ignore = "measurement: reads real_exes/, compiles thousands of blocks"]
fn reference_compile_cost_per_block() {
    use crate::jit::baseline::{Tier0Fit, tier0_fit};
    use crate::jit::block::decode_pure_gpr_block;
    use std::sync::atomic::Ordering;

    /// `with_mem` candidates collected from the sweep before sampling down.
    const MAX_CANDIDATES: usize = 4_000;
    /// Blocks actually compiled, after the cap below.
    const MAX_BLOCKS: usize = 1_500;
    /// Compiles spent growing the module/arena before the timed region. The
    /// first version of this measurement reported 15 ms/block because it timed
    /// arena growth, not blocks.
    const GROW_FRACTION: usize = 4; // a quarter of the set

    let exe = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../real_exes/doomretro/doomretro.exe");
    let image = std::fs::read(&exe).unwrap_or_else(|e| panic!("read {}: {e}", exe.display()));
    let (text_va, text) = text_section(&image);
    let page = text_va & !0xfff;
    let offset_in_page = usize::try_from(text_va - page).expect("page offset");
    let map_size = (offset_in_page + text.len() + 0xfff) & !0xfff;

    let mut cpu = Box::new(JitCpu::open_x86_64());
    map_code_region(cpu.as_mut(), page, map_size);
    cpu.mem_write(text_va, text).expect("text bytes");

    // --- decode sweep (untimed) ------------------------------------------
    //
    // The sweep walks **instruction boundaries** with iced and decodes a block
    // at each one, which is the shape a first-sight run produces (every RIP a
    // run reaches is an instruction boundary). An earlier version advanced by
    // the decoded block's `end_rip` and stepped one byte past anything that
    // failed to decode; one desync poisoned the rest of the walk and it found 16
    // `with_mem` blocks in a 1.4 MB `.text` instead of thousands.
    let decoded: Vec<(u64, crate::jit::block::BlockKind)> = {
        let mem = cpu.shared.mem.read().expect("mem lock");
        let mut out = Vec::new();
        let mut boundaries = 0_u64;
        let mut decoder =
            iced_x86::Decoder::with_ip(64, text, text_va, iced_x86::DecoderOptions::NONE);
        while decoder.can_decode() && out.len() < MAX_CANDIDATES {
            let instr = decoder.decode();
            if instr.is_invalid() {
                break;
            }
            boundaries = boundaries.saturating_add(1);
            let at = instr.ip();
            let kind = decode_pure_gpr_block(&mem, None, at);
            if tier0_fit(&kind) == Tier0Fit::WithMem {
                out.push((at, kind));
            }
        }
        eprintln!(
            "tier0 reference: swept {boundaries} instruction boundaries of a {} KiB .text",
            text.len() / 1024
        );
        out
    };
    let found = decoded.len();
    let stride = found.div_ceil(MAX_BLOCKS).max(1);
    let set: Vec<(u64, crate::jit::block::BlockKind)> = decoded
        .into_iter()
        .step_by(stride)
        .take(MAX_BLOCKS)
        .collect();
    assert!(!set.is_empty(), "the sweep found no with_mem block");

    let inv_gen = cpu.shared.invalidate_gen.load(Ordering::Acquire);
    let grow = (set.len() / GROW_FRACTION).max(1);
    for (at, kind) in set.iter().take(grow) {
        let _ = cpu
            .shared
            .compile_from_kind_shared(&[], *at, kind.clone(), inv_gen);
    }

    // --- timed region ----------------------------------------------------
    let mut us = Vec::with_capacity(set.len());
    let mut insns = Vec::with_capacity(set.len());
    let mut failed = 0_u32;
    let mut halves = [Vec::new(), Vec::new()];
    let timed: Vec<&(u64, crate::jit::block::BlockKind)> = set.iter().skip(grow).collect();
    for (i, (at, kind)) in timed.iter().enumerate() {
        let start = std::time::Instant::now();
        let compiled = cpu
            .shared
            .compile_from_kind_shared(&[], *at, kind.clone(), inv_gen);
        let dt = start.elapsed().as_secs_f64() * 1e6;
        match compiled {
            Some(c) => {
                us.push(dt);
                insns.push(c.insn_count);
                halves[usize::from(i >= timed.len() / 2)].push(dt);
            }
            None => failed = failed.saturating_add(1),
        }
    }

    let load = std::process::Command::new("uptime")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map_or_else(
            || "?".to_string(),
            |s| {
                s.split("load averages: ")
                    .nth(1)
                    .unwrap_or("?")
                    .trim()
                    .to_string()
            },
        );
    let med = |v: &mut Vec<f64>| -> f64 {
        v.sort_by(f64::total_cmp);
        if v.is_empty() { 0.0 } else { v[v.len() / 2] }
    };
    let mean = us.iter().sum::<f64>() / (us.len().max(1) as f64);
    let min = us.iter().copied().fold(f64::INFINITY, f64::min);
    let p90 = {
        let mut v = us.clone();
        v.sort_by(f64::total_cmp);
        v.get(v.len() * 9 / 10).copied().unwrap_or(0.0)
    };
    let median = med(&mut us);
    let first = med(&mut halves[0]);
    let second = med(&mut halves[1]);
    let insn_median = med(&mut insns.iter().map(|&n| f64::from(n)).collect::<Vec<_>>());
    eprintln!(
        "tier0 reference (real .text): with_mem blocks found={found} sampled={} timed={} \
         failed={failed}\n           compile us/block: min={min:.1} median={median:.1} mean={mean:.1} p90={p90:.1}\n           insns/block median={insn_median:.0}  first-half median={first:.1} second-half={second:.1} \
         (a big gap means arena growth, not block cost)\n  host load averages: {load}",
        set.len(),
        us.len(),
    );
    assert!(median > 0.0, "the measurement must produce a number");
}

/// Pull `(VA, bytes)` of `.text` out of a PE image.
///
/// Deliberately minimal: just enough header walking to find the section, so the
/// measurement does not drag `wie-pe` into `wie-cpu`'s test build.
fn text_section(image: &[u8]) -> (u64, &[u8]) {
    fn u16_at(b: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([b[at], b[at + 1]])
    }
    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
    }
    let pe = u32_at(image, 0x3c) as usize;
    assert_eq!(&image[pe..pe + 4], b"PE\0\0", "not a PE image");
    let sections = u16_at(image, pe + 6) as usize;
    let opt_size = u16_at(image, pe + 20) as usize;
    let opt = pe + 24;
    assert_eq!(u16_at(image, opt), 0x20b, "expected PE32+");
    let mut image_base = [0_u8; 8];
    image_base.copy_from_slice(&image[opt + 24..opt + 32]);
    let image_base = u64::from_le_bytes(image_base);
    let table = opt + opt_size;
    for i in 0..sections {
        let s = table + i * 40;
        if &image[s..s + 8] == b".text\0\0\0" {
            let va = u32_at(image, s + 12) as u64;
            let raw_size = u32_at(image, s + 16) as usize;
            let raw_ptr = u32_at(image, s + 20) as usize;
            return (image_base + va, &image[raw_ptr..raw_ptr + raw_size]);
        }
    }
    panic!("no .text section");
}
