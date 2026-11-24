//! A refused block must not poison the next compile.
//!
//! `lower::compile_block` reuses the engine's `ctx.func` and `func_ctx` across
//! blocks. The success path finalizes the Cranelift builder and clears the
//! context; a lowering that returns `Err` part-way used to leave both holding
//! the abandoned function, and Cranelift then panicked on the *next* block
//! compiled on that engine (`ssa.rs: you have declared a non-branch instruction
//! as a predecessor to a block!`). Refusals are routine — an operand shape the
//! emitter does not cover, a verifier text — so this is a real guest crash, not
//! a debug assertion: `real_exes/2048.exe` died a few moves into a game, which
//! is how it was found.
//!
//! The test refuses one block (a `div` form the gate admits and `lower_div`
//! rejects), then compiles and runs a trivial block on the same engine and
//! checks the result. Without the reset in `compile_block` the second compile
//! panics inside Cranelift rather than failing an assertion here.

use crate::ThreadContext;
use crate::exec::StepResult;
use crate::mem::protect;
use crate::mem::{MEM_COMMIT, MEM_RESERVE};
use crate::{CpuEngine, JitCpu};

/// Distinct page per test (the interpreter's decode cache is keyed by RIP).
const REFUSED_BASE: u64 = 0x6600_0000;
const GOOD_BASE: u64 = 0x6601_0000;

fn map(cpu: &mut dyn CpuEngine, base: u64, bytes: &[u8]) {
    cpu.virtual_alloc(
        base,
        0x2000,
        MEM_RESERVE | MEM_COMMIT,
        protect::PAGE_EXECUTE_READWRITE,
    )
    .expect("map test pages");
    cpu.mem_write(base, bytes).expect("write guest bytes");
}

#[test]
fn a_refused_block_does_not_break_the_next_compile() {
    let mut cpu = JitCpu::open_x86_64();
    let stack = GOOD_BASE + 0x1000;

    // `div word [rbx]; ret` — the gate admits `div` (its memory form carries no
    // size check) and `lower_div` refuses everything below 32 bits, so this
    // block fails *inside* the lowering, after the builder exists.
    map(&mut cpu, REFUSED_BASE, &[0x66, 0xF7, 0x33, 0xC3]);
    // `mov eax,1; ret` — the block compiled right after the refusal.
    map(&mut cpu, GOOD_BASE, &[0xB8, 0x01, 0x00, 0x00, 0x00, 0xC3]);

    let mut ctx = ThreadContext {
        rip: REFUSED_BASE,
        ..ThreadContext::default()
    };
    ctx.gpr[4] = stack + 0x800;

    // The poisoning surfaces on whichever engine compiles: in the guest that
    // found it (`real_exes/2048.exe`) the panic was on `wie-jit-bg-0`, so start
    // the background compiler and let it take the refused block.
    let shared = std::sync::Arc::clone(cpu.shared_jit());
    shared.ensure_bg_worker();

    // Enough visits for the engine to try (and refuse) the block.
    for _ in 0..64 {
        cpu.restore_thread_context(&ctx);
        cpu.on_thread_switch();
        let _ = cpu.step_one().expect("step refused block");
    }

    ctx.rip = GOOD_BASE;
    ctx.gpr[0] = 0;
    let mut rax = 0;
    for _ in 0..64 {
        cpu.restore_thread_context(&ctx);
        cpu.on_thread_switch();
        let (result, _) = cpu.step_one().expect("step good block");
        assert!(
            matches!(result, StepResult::Continue),
            "the good block must run: {result:?}"
        );
        rax = cpu.read_rax().expect("read rax");
    }
    assert_eq!(
        rax, 1,
        "the block compiled after a refused one must execute correctly"
    );
    assert!(
        cpu.stats()
            .exec
            .cache_hits
            .saturating_add(cpu.stats().exec.jit_insns)
            > 0,
        "and it must have been compiled, not interpreted"
    );
}
