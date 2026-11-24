//! Differential coverage for the near **indirect call** (`call r/m64`).
//!
//! Only `call rel32` was a lowerable terminator before this, so every
//! `call rax` / `call [rbx+8]` — C++ virtual calls, function pointers, import
//! thunks — refused its whole block and ran on the interpreter: 82.5 % of
//! 7-Zip's interpreted instructions, measured with `WIE_EXEC_TRACE=1`.
//!
//! The change is small because the block exit already carries a *runtime* RIP
//! (`ret` leaves a block that way); what it can get wrong is guest state, so
//! each shape runs on both engines from an identical start and the two are
//! compared on everything a call may touch — RIP, RSP, the pushed return
//! address, RFLAGS — and on one word it must not touch at all.
//!
//! Each byte sequence gets its own const page: the interpreter's decode cache
//! is keyed by RIP, so two tests sharing an address would silently execute the
//! first test's instruction (see `jit::tests::harness`'s module comment). The
//! page is a const rather than a fresh allocation because both engines have to
//! map the *same* address for the snapshots to be comparable.

use crate::ThreadContext;
use crate::exec::StepResult;
use crate::mem::protect;
use crate::mem::{MEM_COMMIT, MEM_RESERVE};
use crate::regs::Rflags;
use crate::{CpuEngine, CpuError, IcedCpu, JitCpu};

/// One page pair per test, clear of the range `jit::tests` allocates from
/// (`0x5000_0000` upward) and of the runtime's planted stub regions.
const REG_CALL_BASE: u64 = 0x6500_0000;
const MEM_CALL_BASE: u64 = 0x6501_0000;
const POP_CALL_BASE: u64 = 0x6502_0000;

/// RSP start offset inside the stack page (leaves room for the push).
const STACK_RSP: u64 = 0x800;
/// A word far from the call's stack traffic that must survive untouched.
const SCRATCH_OFF: u64 = 0xF00;
const SCRATCH_VALUE: u64 = 0xA5A5_5A5A_1234_5678;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Backend {
    Iced,
    Jit,
}

/// Guest state a near call may change, plus what it must leave alone.
#[derive(Debug, PartialEq, Eq)]
struct State {
    rip: u64,
    rsp: u64,
    rsp_in: u64,
    rflags: Rflags,
    rflags_in: Rflags,
    /// The 8 bytes RSP points at after the call: the return address.
    pushed: u64,
    scratch_in: u64,
    scratch_out: u64,
}

/// What one run of 64 identical trials observed.
struct Outcome {
    state: State,
    /// True when at least one trial retired a **compiled** block; the guard
    /// that keeps this test from passing while exercising only the interpreter.
    compiled: bool,
}

fn map(cpu: &mut dyn CpuEngine, base: u64) {
    cpu.virtual_alloc(
        base,
        0x2000,
        MEM_RESERVE | MEM_COMMIT,
        protect::PAGE_EXECUTE_READWRITE,
    )
    .expect("map test pages");
}

fn write(cpu: &mut dyn CpuEngine, at: u64, bytes: &[u8]) {
    cpu.mem_write(at, bytes).expect("write guest bytes");
}

fn read_u64(cpu: &mut dyn CpuEngine, at: u64) -> u64 {
    let mut buf = [0_u8; 8];
    cpu.mem_read(at, &mut buf).expect("read guest word");
    u64::from_le_bytes(buf)
}

/// Run `bytes` (an indirect call) 64 times from the same start state.
///
/// Repetition is what promotes the block: the JIT compiles only once a block is
/// hot, so a single interpreted trial would never reach the lowering under
/// test. `prep` plants whatever the encoding reads (a register, a memory slot)
/// and runs before every trial.
fn trials(
    backend: Backend,
    base: u64,
    bytes: &[u8],
    prep: impl Fn(&mut dyn CpuEngine, u64, &mut ThreadContext),
) -> Outcome {
    match backend {
        Backend::Iced => {
            let mut cpu = IcedCpu::open_x86_64();
            run(&mut cpu, base, bytes, prep, |c: &mut IcedCpu| {
                c.step_once_result().map(|r| (r, false))
            })
        }
        Backend::Jit => {
            let mut cpu = JitCpu::open_x86_64();
            run(&mut cpu, base, bytes, prep, |c: &mut JitCpu| {
                let before = c
                    .stats()
                    .exec
                    .cache_hits
                    .saturating_add(c.stats().exec.jit_insns);
                c.step_one().map(|(r, _)| {
                    let after = c
                        .stats()
                        .exec
                        .cache_hits
                        .saturating_add(c.stats().exec.jit_insns);
                    (r, after > before)
                })
            })
        }
    }
}

fn run<C: CpuEngine>(
    cpu: &mut C,
    base: u64,
    bytes: &[u8],
    prep: impl Fn(&mut dyn CpuEngine, u64, &mut ThreadContext),
    step: impl Fn(&mut C) -> Result<(StepResult, bool), CpuError>,
) -> Outcome {
    map(cpu, base);
    write(cpu, base, bytes);
    let stack = base + 0x1000;

    let mut ctx = ThreadContext {
        rip: base,
        ..ThreadContext::default()
    };
    ctx.gpr[4] = stack + STACK_RSP;

    let mut out = Outcome {
        state: State {
            rip: 0,
            rsp: 0,
            rsp_in: 0,
            rflags: Rflags::DEFAULT,
            rflags_in: Rflags::DEFAULT,
            pushed: 0,
            scratch_in: 0,
            scratch_out: 0,
        },
        compiled: false,
    };
    for _ in 0..64 {
        // `prep` writes memory and may set registers on `ctx`; restoring after
        // it makes `ctx` authoritative for RIP/RSP either way.
        prep(cpu, base, &mut ctx);
        cpu.restore_thread_context(&ctx);
        cpu.on_thread_switch();

        let start = cpu.snapshot_thread_context();
        let scratch_in = read_u64(cpu, stack + SCRATCH_OFF);
        let (result, compiled) = step(cpu).expect("step");
        assert!(
            matches!(result, StepResult::Continue),
            "a call continues, got {result:?}"
        );
        out.compiled |= compiled;

        let after = cpu.snapshot_thread_context();
        out.state = State {
            rip: after.rip,
            rsp: after.gpr[4],
            rsp_in: start.gpr[4],
            rflags: after.rflags,
            rflags_in: start.rflags,
            pushed: read_u64(cpu, after.gpr[4]),
            scratch_in,
            scratch_out: read_u64(cpu, stack + SCRATCH_OFF),
        };
    }
    out
}

/// Shared assertions, so each shape pins the same contract.
fn check_call(state: &State, next_ip: u64, target: u64) {
    assert_eq!(state.rip, target, "the call exits at the target it read");
    assert_eq!(
        state.pushed, next_ip,
        "a call pushes exactly the address of the instruction after it"
    );
    assert_eq!(
        state.rsp,
        state.rsp_in.wrapping_sub(8),
        "a call pops nothing and pushes one 8-byte slot"
    );
    assert_eq!(
        state.rflags, state.rflags_in,
        "a call does not modify RFLAGS"
    );
    assert_eq!(
        state.scratch_in, state.scratch_out,
        "a call must not touch unrelated guest memory"
    );
    assert_eq!(
        state.scratch_in, SCRATCH_VALUE,
        "the scratch word must have been planted"
    );
}

/// `call rcx` (FF D1): the register form a vtable / function-pointer call uses.
#[test]
fn call_register_target_matches_the_interpreter() {
    let target = REG_CALL_BASE + 0x200;
    let mut states = Vec::new();
    for backend in [Backend::Iced, Backend::Jit] {
        let out = trials(backend, REG_CALL_BASE, &[0xFF, 0xD1], |cpu, base, ctx| {
            ctx.gpr[1] = target; // RCX
            write(
                cpu,
                base + 0x1000 + SCRATCH_OFF,
                &SCRATCH_VALUE.to_le_bytes(),
            );
        });
        if matches!(backend, Backend::Jit) {
            assert!(
                out.compiled,
                "the JIT must have executed this call as compiled code, not on the interpreter"
            );
        }
        check_call(&out.state, REG_CALL_BASE + 2, target);
        states.push(out.state);
    }
    assert_eq!(
        states[0], states[1],
        "iced and jit must agree field by field"
    );
}

/// `call qword ptr [rbx]` (FF 13): the memory form — a vtable slot, an import
/// thunk table, and (RIP-relative) the `call [rip+disp]` most binaries use.
#[test]
fn call_memory_target_matches_the_interpreter() {
    let target = MEM_CALL_BASE + 0x200;
    let slot = MEM_CALL_BASE + 0x800;
    let mut states = Vec::new();
    for backend in [Backend::Iced, Backend::Jit] {
        let out = trials(backend, MEM_CALL_BASE, &[0xFF, 0x13], |cpu, base, ctx| {
            ctx.gpr[3] = slot; // RBX → the target slot
            write(cpu, slot, &target.to_le_bytes());
            write(
                cpu,
                base + 0x1000 + SCRATCH_OFF,
                &SCRATCH_VALUE.to_le_bytes(),
            );
        });
        if matches!(backend, Backend::Jit) {
            assert!(
                out.compiled,
                "the JIT must have executed this call as compiled code, not on the interpreter"
            );
        }
        check_call(&out.state, MEM_CALL_BASE + 2, target);
        states.push(out.state);
    }
    assert_eq!(
        states[0], states[1],
        "iced and jit must agree field by field"
    );
}

/// The operand is read through the **pre-push** RSP: `call [rsp]` must call the
/// address the stack held before the return address was written over it.
///
/// This is the one ordering a lowering can plausibly get wrong, and the
/// interpreter spells the same rule out in `exec_call`.
#[test]
fn call_through_rsp_reads_the_pre_push_stack() {
    let target = POP_CALL_BASE + 0x200;
    // `call qword ptr [rsp]`: FF /2 with ModRM 0x14 and SIB 0x24 ([rsp], no index).
    let mut states = Vec::new();
    for backend in [Backend::Iced, Backend::Jit] {
        let out = trials(
            backend,
            POP_CALL_BASE,
            &[0xFF, 0x14, 0x24],
            |cpu, base, ctx| {
                let rsp = ctx.gpr[4];
                write(cpu, rsp, &target.to_le_bytes());
                write(
                    cpu,
                    base + 0x1000 + SCRATCH_OFF,
                    &SCRATCH_VALUE.to_le_bytes(),
                );
            },
        );
        if matches!(backend, Backend::Jit) {
            assert!(
                out.compiled,
                "the JIT must have executed this call as compiled code, not on the interpreter"
            );
        }
        check_call(&out.state, POP_CALL_BASE + 3, target);
        states.push(out.state);
    }
    assert_eq!(
        states[0], states[1],
        "iced and jit must agree field by field"
    );
}

/// The gate now admits `call r/m64`, so the refusal path must not still see it:
/// the operand-form class that used to swallow it (`term_call`) is gone for
/// these encodings, and the 16-bit push form stays refused on purpose.
#[test]
fn admission_of_indirect_calls_is_the_table_not_the_encoder() {
    use super::opclass;
    use iced_x86::{Code, Decoder, DecoderOptions, Instruction, Mnemonic};

    fn decode(bytes: &[u8], ip: u64) -> Instruction {
        let mut buf = [0_u8; 15];
        buf[..bytes.len()].copy_from_slice(bytes);
        Decoder::with_ip(64, &buf, ip, DecoderOptions::NONE).decode()
    }

    // Admitted: the 64-bit register and memory forms. `term_of` is the gate a
    // terminator passes (a body instruction would be `admits`), so this is the
    // pair that decides whether the block compiles or is marked never.
    for bytes in [
        &[0xFF_u8, 0xD1][..],
        &[0xFF, 0x13][..],
        &[0xFF, 0x14, 0x24][..],
    ] {
        let instr = decode(bytes, 0x1000);
        assert!(
            opclass::term_of(&instr).is_some(),
            "{bytes:02X?} must classify as a block terminator"
        );
    }

    // Refused on purpose: a **far** call (`FF /3`, an m16:64 pointer) is not
    // this shape — the JIT's exit carries one 8-byte target, not a segment and
    // an offset.
    let far = decode(&[0xFF, 0x1B], 0x1000); // call far [rbx]
    assert_eq!(far.mnemonic(), Mnemonic::Call);
    assert!(
        opclass::term_of(&far).is_none(),
        "a far call must stay on the interpreter"
    );

    // And the fact that surprised this test's first draft: in 64-bit mode the
    // operand size of a near call is forced to 64 bits, so the `66`-prefixed
    // encoding is still `Call_rm64` with an 8-byte push. Pinning it here means
    // an iced upgrade that starts reporting `Call_rm16` shows up as this test
    // failing rather than as a silently narrower push in the JIT.
    let narrow = decode(&[0x66, 0xFF, 0xD1], 0x1000);
    assert_eq!(narrow.code(), Code::Call_rm64);
    assert_eq!(narrow.stack_pointer_increment(), -8);
    assert!(
        opclass::term_of(&narrow).is_some(),
        "iced reports this as a 64-bit indirect call, so the JIT lowering it is correct"
    );
}
