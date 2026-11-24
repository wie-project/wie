//! Region-emission tests: a loop region as **one** native function.
//!
//! These exercise [`super::super::lower::try_compile_region`] through the exact
//! entry point `step_one` uses (`JitCpu::try_compile`), on real guest bytes, and
//! compare the result against the `iced` interpreter — because a region's failure
//! mode is *silent wrong code*, not a crash: the whole point of the shape is that
//! it stops re-synchronising `JitCtx` with the guest register file, so a mistake
//! there produces plausible-looking register values that no assertion of "it ran"
//! would catch.
//!
//! Four things are pinned, and each of them is a claim the emitter makes:
//!
//! 1. **The shape is one function.** `try_compile` returns a `CompiledBlock` whose
//!    `insn_count` is the *region's* instruction count and whose `guest_end` spans
//!    every block — against the per-block translation of the same RIP, which is
//!    exactly one block. That is also the code-cache difference (the region owns a
//!    wider guest range, so range-selective invalidation sees it).
//! 2. **The back edge is native.** A loop that iterates counts down in registers
//!    and takes **zero** chain hops: `chain.hops` is the counter that separates
//!    "one block singularly misinterpreted as a chain" from "one function with a
//!    `jump`", and `exec.jit_insns` is exact for this fixture (3 guest instructions
//!    × 3 iterations = 9), which pins the trip counter too.
//! 3. **Flags cross the back edge as architectural flags.** The fixture's `jg` sits
//!    in the second block and reads the `sub`'s flags from the first: block-to-block
//!    flag flow through the loop-carried carrier, which is soundness rule 3 of
//!    `region.rs`.
//! 4. **A fault inside a region leaves the same frame the interpreter has.** The
//!    faulting IP is the guest's own instruction IP and every register matches —
//!    this is the "faults must leave the region cleanly with the file brought up to
//!    date" contract, and the reason the exit writes the whole file.
//!
//! Per-test code VAs are distinct consts (the harness's one rule: a distinct VA per
//! distinct byte sequence, because the interpreter's decode cache is keyed by
//! `(rip, mem_generation)` process-wide).

use super::harness::{Backend, Engine, map_code_page, open};
use super::*;
use crate::ThreadContext;
use crate::exec::StepResult;

/// `sub rcx, 1` in the first block, `jg` back to it from the second.
///
/// ```text
/// A +0x00: sub rcx, 1        ; 48 FF C9        writes flags
///   +0x03: jmp B             ; EB 06           -> +0x0B
///   +0x05: nop ×6            ; unreachable padding (an inline `jmp` to its own
///                            ;   fallthrough would be merged into one block)
/// B +0x0B: jg A              ; 7F F3           reads A's `sub` flags — the
///                            ;   cross-block flag flow, through the back edge
///   +0x0D: ud2               ; exit target (stops region exploration)
/// ```
///
/// The only live-in is RCX (read by `sub` before it is written), and RFLAGS is not
/// a live-in: `sub` must define it before B reads it.
const CROSS_FLAGS_LOOP: [u8; 0x0F] = [
    0x48, 0xFF, 0xC9, // sub rcx, 1
    0xEB, 0x06, // jmp +0x0B
    0x90, 0x90, 0x90, 0x90, 0x90, 0x90, // padding
    0x7F, 0xF3, // jg +0x00
    0x0F, 0x0B, // ud2
];
const CROSS_FLAGS_ENTRY: u64 = 0x10b0_0000;
const CROSS_FLAGS_EXIT: u64 = CROSS_FLAGS_ENTRY + 0x0D;

/// `add qword [rdi + rcx*4], 1` in the first block, `jg` back from the second.
///
/// ```text
/// A +0x00: add qword [rdi+rcx*4], 1   ; 48 83 04 8F 01   read-modify-write
///   +0x05: sub rcx, 1                 ; 48 FF C9
///   +0x08: jmp B                      ; EB 06 -> +0x10
///   +0x0A: nop ×6
/// B +0x10: jg A                       ; 7F EE
///   +0x12: ud2                        ; exit target
/// ```
///
/// Live-ins here are RCX (the `sub`) and RDI (the address base) — an actual
/// pointer carried across a native back edge, which is the case that has to survive
/// the *exit* path's writeback and the entry's load.
const MEM_LOOP: [u8; 0x14] = [
    0x48, 0x83, 0x04, 0x8F, 0x01, // add qword [rdi+rcx*4], 1
    0x48, 0xFF, 0xC9, // sub rcx, 1
    0xEB, 0x06, // jmp +0x10
    0x90, 0x90, 0x90, 0x90, 0x90, 0x90, // padding
    0x7F, 0xEE, // jg +0x00
    0x0F, 0x0B, // ud2
];
const MEM_LOOP_ENTRY: u64 = 0x10b1_0000;
const MEM_LOOP_EXIT: u64 = MEM_LOOP_ENTRY + 0x12;
const MEM_LOOP_DATA: u64 = 0x10b2_0000;

/// A loop containing a `call`: formation refuses it (`RegionRefusal::Call`), so the
/// per-block path must still compile the RIP.
///
/// ```text
/// +0x00: call +0x100     ; E8 FB 00 00 00
/// +0x05: jmp +0x00       ; EB F9
/// ```
const CALL_LOOP: [u8; 7] = [0xE8, 0xFB, 0x00, 0x00, 0x00, 0xEB, 0xF9];

/// `sub rcx, 1; ret` — straight-line, so there is no region to form.
const STRAIGHT_ENTRY: u64 = 0x10b4_0000;
const CALL_LOOP_ENTRY: u64 = 0x10b3_0000;

/// A JIT engine with the per-engine region latch set, and `bytes` at `base`.
///
/// The latch is a field on `JitShared` rather than the `WIE_JIT_REGION` env var for
/// the reason documented there: `JitConfig` is a process-wide `OnceLock`, so the
/// unit suite cannot flip a knob at run time, and must not — other tests in this
/// binary would silently get regions too.
fn jit_with(base: u64, bytes: &[u8], region: bool) -> Box<JitCpu> {
    let mut cpu = Box::new(JitCpu::open_x86_64());
    map_code_page(cpu.as_mut(), base);
    cpu.mem_write(base, bytes).expect("write fixture bytes");
    cpu.shared_jit()
        .region_force
        .store(region, std::sync::atomic::Ordering::Relaxed);
    // Register pressure is what makes a block path visible in `chain.hops`; keep
    // the chain machinery at its default so the hop counter is meaningful.
    cpu
}

/// Compile `rip` the way `step_one` does when a block is promoted — through
/// `try_compile`, which is where the region decision lives — and install the
/// result so the next `step_one` runs it.
fn compile_and_install(cpu: &mut JitCpu, rip: u64) -> lower::CompiledBlock {
    let compiled = cpu.try_compile(rip).expect("compile the fixture");
    cpu.insert_ready(rip, compiled);
    compiled
}

/// Step both engines until each one reaches `exit_rip`, then compare the whole
/// thread context.
///
/// The JIT reaches it in one `step_one` (the region runs the loop and leaves with
/// `rip` = the exit target); the interpreter needs one step per instruction. Bounding
/// both by "until rip == exit" — rather than by a step count — is what makes this a
/// comparison of *the same guest work*.
fn run_to_exit(engine: &mut Engine, exit_rip: u64, base: u64) -> ThreadContext {
    let mut steps = 0_usize;
    loop {
        let rip = engine.cpu().snapshot_thread_context().rip;
        assert_eq!(
            rip & !0xFFF,
            base & !0xFFF,
            "execution left the fixture page: rip={rip:#x}"
        );
        if rip == exit_rip {
            return engine.cpu().snapshot_thread_context();
        }
        steps = steps.saturating_add(1);
        assert!(steps < 4096, "fixture never reached {exit_rip:#x}");
        let _ = engine.run_rmw();
    }
}

/// The counters that separate a native back edge from a chain hop.
fn hop_counters(cpu: &JitCpu) -> (u64, u64) {
    let s = cpu.stats();
    (s.chain.hops, s.exec.jit_insns)
}

// ── 1. the shape ────────────────────────────────────────────────────────

/// The same RIP compiles to a **region** (36-instruction-style: every block, one
/// function, a guest range spanning both) with the latch on, and to exactly one
/// block with it off.
///
/// This is the assertion the measurement rests on: if the latch silently did
/// nothing, every other test here would still pass by exercising the per-block path
/// and would prove nothing about regions.
#[test]
fn the_region_decision_changes_what_one_rip_compiles_to() {
    let mut region = jit_with(CROSS_FLAGS_ENTRY, &CROSS_FLAGS_LOOP, true);
    let compiled = compile_and_install(region.as_mut(), CROSS_FLAGS_ENTRY);
    assert_eq!(
        compiled.insn_count, 3,
        "the region is all three guest instructions (sub, jmp, jg)"
    );
    assert_eq!(
        compiled.guest_start, CROSS_FLAGS_ENTRY,
        "a region is keyed by its entry"
    );
    assert_eq!(
        compiled.guest_end,
        CROSS_FLAGS_ENTRY + 0x0D,
        "the region owns the guest bytes of *both* blocks (up to the `jg`'s end, \
         not the `ud2` exit target), so range-selective invalidation drops it when \
         either block's bytes are patched"
    );
    assert!(
        compiled.func_id.is_some(),
        "a region is a real Cranelift function, not a hand-written stub"
    );

    let mut block = jit_with(CROSS_FLAGS_ENTRY, &CROSS_FLAGS_LOOP, false);
    let single = compile_and_install(block.as_mut(), CROSS_FLAGS_ENTRY);
    assert_eq!(
        single.insn_count, 2,
        "without the latch this RIP is the first block only (sub + jmp)"
    );
    assert_eq!(
        single.guest_end,
        CROSS_FLAGS_ENTRY + 0x05,
        "a block owns only its own bytes"
    );
}

// ── 2. native back edge, exact trip count ───────────────────────────────

/// Three iterations of a three-instruction loop, run as one function: **zero** chain
/// hops and exactly nine retired guest instructions.
///
/// `chain.hops` is the counter that makes the structural claim measurable — the
/// per-block shape spends one hop per iteration (and one load-in/store-back per hop),
/// the region spends none — and `exec.jit_insns` being *exactly* 9 pins the trip
/// counter: seeded with the region's static length and bumped once per native back
/// edge, which is the only way nine comes out without per-instruction accounting.
#[test]
fn a_region_iterates_without_chain_hops() {
    let mut cpu = jit_with(CROSS_FLAGS_ENTRY, &CROSS_FLAGS_LOOP, true);
    cpu.thread.regs.set_gpr(1, 3); // RCX = 3
    cpu.write_rip(CROSS_FLAGS_ENTRY).expect("set rip");
    let compiled = compile_and_install(cpu.as_mut(), CROSS_FLAGS_ENTRY);
    assert_eq!(compiled.insn_count, 3, "precondition: this is the region");

    let (result, retired) = cpu.step_one().expect("step");
    assert!(
        matches!(result, StepResult::Continue),
        "the loop must leave through its exit, not fault: {result:?}"
    );

    assert_eq!(
        cpu.thread.regs.gpr(1),
        0,
        "the loop must count RCX down to zero"
    );
    assert_eq!(
        cpu.thread.regs.rip, CROSS_FLAGS_EXIT,
        "the region sets rip to the exit target"
    );
    let (hops, insns) = hop_counters(&cpu);
    assert_eq!(
        hops, 0,
        "a native back edge takes no chain hop; hops={hops} means the loop was \
         still hopping between per-block functions"
    );
    assert_eq!(
        insns, 9,
        "3 guest instructions x 3 iterations; anything else means the trip \
         counter charged the wrong number of trips"
    );
    assert_eq!(
        retired, 9,
        "the dispatcher must be charged the same dynamic count the frame charged"
    );
}

// ── 3. the differentials ────────────────────────────────────────────────

/// The cross-block-flag loop, region-compiled, against the interpreter.
///
/// The `jg` in block B consumes flags produced by the `sub` in block A — across the
/// native back edge — so this is the test for soundness rule 3: the loop-carried
/// RFLAGS carrier has to hold architectural flags at every block boundary, or the
/// loop takes the wrong exit.
#[test]
fn a_region_loop_matches_the_interpreter() {
    for rcx in [0_u64, 1, 3, 17] {
        let mut results: Vec<ThreadContext> = Vec::new();
        for backend in [Backend::Iced, Backend::Jit] {
            let mut engine = open(backend);
            if let Engine::Jit(cpu) = &mut engine {
                cpu.shared_jit()
                    .region_force
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            {
                let cpu = engine.cpu();
                cpu.virtual_alloc(
                    CROSS_FLAGS_ENTRY,
                    0x1000,
                    MEM_RESERVE | MEM_COMMIT,
                    protect::PAGE_EXECUTE_READWRITE,
                )
                .expect("map the fixture page");
                cpu.mem_write(CROSS_FLAGS_ENTRY, &CROSS_FLAGS_LOOP)
                    .expect("write the fixture");
                let mut ctx = cpu.snapshot_thread_context();
                ctx.gpr[1] = rcx;
                ctx.rip = CROSS_FLAGS_ENTRY;
                cpu.restore_thread_context(&ctx);
            }
            if let Engine::Jit(cpu) = &mut engine {
                compile_and_install(cpu.as_mut(), CROSS_FLAGS_ENTRY);
            }
            results.push(run_to_exit(
                &mut engine,
                CROSS_FLAGS_EXIT,
                CROSS_FLAGS_ENTRY,
            ));
        }
        let (iced, jit) = (&results[0], &results[1]);
        assert_eq!(
            jit.rip, iced.rip,
            "rcx={rcx}: exit RIP differs (region vs interpreter)"
        );
        assert_eq!(
            jit.gpr, iced.gpr,
            "rcx={rcx}: the register file differs — a region exit writes back \
             every slot, so a mismatch here is a missing or wrong writeback"
        );
        assert_eq!(
            jit.rflags, iced.rflags,
            "rcx={rcx}: the flags leaving the loop differ"
        );
    }
}

/// A read-modify-write memory operand inside the region, with the base pointer as a
/// live-in: `add qword [rdi+rcx*4], 1`.
///
/// This is the memory path under the region's *stale-file* premise — the refill
/// probe is hoisted once at region entry instead of per hop — plus a live-in pointer
/// that has to be carried in SSA across the back edge.
#[test]
fn a_region_with_a_memory_operand_matches_the_interpreter() {
    let mut results = Vec::new();
    for backend in [Backend::Iced, Backend::Jit] {
        let mut engine = open(backend);
        if let Engine::Jit(cpu) = &mut engine {
            cpu.shared_jit()
                .region_force
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        {
            let cpu = engine.cpu();
            cpu.virtual_alloc(
                MEM_LOOP_ENTRY,
                0x1000,
                MEM_RESERVE | MEM_COMMIT,
                protect::PAGE_EXECUTE_READWRITE,
            )
            .expect("map the fixture page");
            cpu.virtual_alloc(
                MEM_LOOP_DATA,
                0x1000,
                MEM_RESERVE | MEM_COMMIT,
                protect::PAGE_READWRITE,
            )
            .expect("map the data page");
            cpu.mem_write(MEM_LOOP_ENTRY, &MEM_LOOP)
                .expect("write the fixture");
            let mut ctx = cpu.snapshot_thread_context();
            ctx.gpr[1] = 3; // RCX = 3 iterations
            ctx.gpr[7] = MEM_LOOP_DATA; // RDI = base pointer (live-in)
            ctx.rip = MEM_LOOP_ENTRY;
            cpu.restore_thread_context(&ctx);
        }
        if let Engine::Jit(cpu) = &mut engine {
            let compiled = compile_and_install(cpu.as_mut(), MEM_LOOP_ENTRY);
            assert_eq!(compiled.insn_count, 4, "the region is add + sub + jmp + jg");
        }
        let ctx = run_to_exit(&mut engine, MEM_LOOP_EXIT, MEM_LOOP_ENTRY);
        let mut bytes = [0_u8; 16];
        engine
            .cpu()
            .mem_read(MEM_LOOP_DATA, &mut bytes)
            .expect("read the data page back");
        results.push((ctx, bytes));
    }
    let (iced_ctx, iced_bytes) = (&results[0].0, &results[0].1);
    let (jit_ctx, jit_bytes) = (&results[1].0, &results[1].1);
    assert_eq!(jit_ctx.gpr, iced_ctx.gpr, "register file after the loop");
    assert_eq!(jit_ctx.rip, iced_ctx.rip, "exit RIP after the loop");
    assert_eq!(
        jit_bytes.as_slice(),
        iced_bytes.as_slice(),
        "guest memory after the loop (three +1s at offsets 4/8/12)"
    );
    assert_eq!(
        &jit_bytes[..8],
        &[0, 0, 0, 0, 1, 0, 0, 0],
        "the fixture must have written offset 4 and left offset 0 alone"
    );
}

/// A fault on the region's *first* instruction: the frame handed back must be the
/// interpreter's, including the faulting IP and the registers the loop never touched.
///
/// This is the contract that makes the conservative whole-file writeback necessary:
/// the region's `JitCtx` was stale for the loop, so the fault exit is the only place
/// it becomes correct again.
#[test]
fn a_fault_inside_a_region_leaves_the_same_frame_as_the_interpreter() {
    let mut results = Vec::new();
    for backend in [Backend::Iced, Backend::Jit] {
        let mut engine = open(backend);
        if let Engine::Jit(cpu) = &mut engine {
            cpu.shared_jit()
                .region_force
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        {
            let cpu = engine.cpu();
            cpu.virtual_alloc(
                MEM_LOOP_ENTRY,
                0x1000,
                MEM_RESERVE | MEM_COMMIT,
                protect::PAGE_EXECUTE_READWRITE,
            )
            .expect("map the fixture page");
            cpu.mem_write(MEM_LOOP_ENTRY, &MEM_LOOP)
                .expect("write the fixture");
            let mut ctx = cpu.snapshot_thread_context();
            ctx.gpr[1] = 3;
            // An address no test page covers: the `add`'s store must fault.
            ctx.gpr[7] = 0x0000_7fff_fff0_0000;
            // Distinguish the slots the loop never touches: a region that failed to
            // write the whole file back would still look plausible if they were
            // zero, which is what a `JitCtx` starts as.
            for i in 0..16_u64 {
                if i != 1 && i != 7 {
                    ctx.gpr[i as usize] = 0x1000 + i;
                }
            }
            ctx.rip = MEM_LOOP_ENTRY;
            cpu.restore_thread_context(&ctx);
        }
        if let Engine::Jit(cpu) = &mut engine {
            let compiled = compile_and_install(cpu.as_mut(), MEM_LOOP_ENTRY);
            assert_eq!(compiled.insn_count, 4, "precondition: a region");
        }
        let result = engine.run_rmw();
        let ctx = engine.cpu().snapshot_thread_context();
        results.push((result, ctx));
    }
    let (iced_result, iced_ctx) = &results[0];
    let (jit_result, jit_ctx) = &results[1];
    assert!(
        matches!(iced_result, StepResult::InvalidMemory(_)),
        "fixture precondition: the interpreter must fault, got {iced_result:?}"
    );
    assert!(
        matches!(jit_result, StepResult::InvalidMemory(_)),
        "a fault inside a region must be reported, not swallowed: {jit_result:?}"
    );
    assert_eq!(
        jit_ctx.rip, iced_ctx.rip,
        "the faulting guest IP must be the instruction's own IP"
    );
    assert_eq!(
        jit_ctx.gpr, iced_ctx.gpr,
        "the frame at a fault must match the interpreter's, including every \
         register the region never wrote"
    );
}

// ── 4. refusals fall back instead of compiling something wrong ──────────

/// A loop containing a `call` is refused at formation, and the same RIP still
/// compiles — as a block.
///
/// The refusal is the whole safety story for calls (a call leaves the native frame),
/// so the test asserts both halves: the fallback produces a block, and the block is
/// the single `call` (its terminator), not the region.
#[test]
fn a_loop_with_a_call_is_refused_and_falls_back_to_a_block() {
    let mut cpu = jit_with(CALL_LOOP_ENTRY, &CALL_LOOP, true);
    let compiled = compile_and_install(cpu.as_mut(), CALL_LOOP_ENTRY);
    assert_eq!(
        compiled.insn_count, 1,
        "the region is refused (RegionRefusal::Call), so this RIP compiles as the \
         block containing just the call terminator"
    );
    assert_eq!(
        compiled.guest_end,
        CALL_LOOP_ENTRY + 5,
        "a refused region must not claim the loop's byte range"
    );
}

/// A RIP that is not a loop header compiles as a block even with the latch on: the
/// region attempt must be inert on straight-line code.
#[test]
fn straight_line_code_is_not_a_region() {
    // `sub rcx, 1; ret` — two lowerable instructions, no cycle. `ret` is what ends
    // the block: without a terminator the decoder would run into the zero-filled
    // rest of the page (`add [rax], al` is lowerable) and hit its 96-insn cap.
    let mut cpu = jit_with(STRAIGHT_ENTRY, &[0x48, 0xFF, 0xC9, 0xC3], true);
    let compiled = compile_and_install(cpu.as_mut(), STRAIGHT_ENTRY);
    assert_eq!(
        compiled.insn_count, 2,
        "a straight-line RIP has no back edge to make native, so it compiles as the \
         one block it is"
    );
    assert_eq!(compiled.guest_end, STRAIGHT_ENTRY + 4);
}
