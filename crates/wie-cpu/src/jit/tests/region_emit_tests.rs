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
use crate::regs::Rflags;

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
    // Self-loops are gated separately in shipping builds
    // (`WIE_JIT_REGION_SELF_LOOP`, default off — the shape loses on DOOM's blit,
    // 171 host instructions per iteration against the block path's 156; neutral
    // within noise on `long_loop`). These tests exist to keep the one-block
    // *emitter* correct, so they ask for it explicitly;
    // `a_self_loop_region_needs_its_own_knob` is the test that pins the gate
    // itself.
    cpu.shared_jit()
        .region_self_loop_force
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

// ── 5. the boot seam: a latch compiles its loop's header ────────────────

/// Compile `rip` as a **block** and install it — the shape the boot tier's
/// first-sighting compile produces (`baseline::try_compile_baseline` →
/// `compile_from_kind_shared`), which never asks about a region at `rip` itself.
fn install_boot_block(cpu: &mut JitCpu, rip: u64) -> lower::CompiledBlock {
    let baked = cpu
        .shared
        .invalidate_gen
        .load(std::sync::atomic::Ordering::Acquire);
    let kind = {
        let mem = cpu.shared.mem.read().expect("mem guard");
        block::decode_pure_gpr_block(&mem, cpu.thread.hooks.as_ref(), rip)
    };
    let compiled = cpu
        .try_compile_from_kind(rip, kind, baked)
        .expect("compile the fixture block");
    cpu.insert_ready(rip, compiled);
    compiled
}

/// The cache entry at `rip`, which is where the dispatcher looks next.
fn ready_entry(cpu: &JitCpu, rip: u64) -> lower::CompiledBlock {
    let cache = cpu.shared.cache.pin();
    match cache.get(&rip).cloned() {
        Some(CacheEntry::Ready(compiled)) => compiled,
        None => panic!("no cache entry at {rip:#x}"),
        Some(_) => panic!("expected a Ready entry at {rip:#x}, got a non-Ready one"),
    }
}

/// **The reachability pin.** Boot mode compiles the first sighting of every block
/// and installs it `Ready`, and nothing re-compiles a `Ready` entry — so a loop
/// header first reached during boot used to stay a per-block, per-iteration hop
/// for the rest of the process. DOOM Retro's entire hot set is in that window,
/// which is why `WIE_JIT_REGION=1` had never had a region to measure on a real
/// guest.
///
/// This pins the fix at the seam that matters rather than at `try_compile` (which
/// always could fold a header it was pointed at): compile the header as a *block*
/// the way boot does, then compile the **latch**, and the header's entry must have
/// become the region. With the latch off it must still be the two-instruction
/// block — the knob must not touch the block path.
#[test]
fn a_latch_compile_folds_the_loop_at_a_boot_compiled_header() {
    for region in [true, false] {
        let mut cpu = jit_with(CROSS_FLAGS_ENTRY, &CROSS_FLAGS_LOOP, region);
        let header = install_boot_block(cpu.as_mut(), CROSS_FLAGS_ENTRY);
        assert_eq!(
            header.insn_count, 2,
            "precondition: the block path's view of the header (sub + jmp)"
        );
        let latch = install_boot_block(cpu.as_mut(), CROSS_FLAGS_ENTRY + 0x0B);
        assert_eq!(latch.insn_count, 1, "the latch is a block too (the `jg`)");

        let folded = ready_entry(cpu.as_ref(), CROSS_FLAGS_ENTRY);
        if region {
            assert_eq!(
                folded.insn_count, 3,
                "the latch compile must fold the whole loop into the header's \
                 entry: 2 + 1 guest instructions"
            );
            assert_eq!(
                folded.guest_start, CROSS_FLAGS_ENTRY,
                "the region is keyed by the header and replaces that entry"
            );
            assert_eq!(
                folded.guest_end,
                CROSS_FLAGS_ENTRY + 0x0D,
                "the region owns the latch's bytes too, so a write to either \
                 block invalidates it"
            );
        } else {
            assert_eq!(
                folded.insn_count, 2,
                "knob off: the header must still be exactly the block the boot \
                 tier installed"
            );
            assert_eq!(folded.guest_end, CROSS_FLAGS_ENTRY + 0x05);
        }
    }
}

/// …and the region that fold installed is *correct*: entered from the header
/// (where a stale chain slot from the earlier block compile used to point), it
/// counts down in registers, takes no chain hop, and leaves the exit `rip` and
/// register file the interpreter has.
///
/// This is the differential for the folded shape specifically: the header was
/// first compiled as a **block**, so the entry state, the chain slot and the
/// cache entry were all built by the block path before the region replaced them.
#[test]
fn the_folded_region_matches_the_interpreter() {
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
            ctx.gpr[1] = 3;
            ctx.rip = CROSS_FLAGS_ENTRY;
            cpu.restore_thread_context(&ctx);
        }
        if let Engine::Jit(cpu) = &mut engine {
            // Boot order: the header first, the latch later. The fold happens on
            // the latch, i.e. after the header is already installed and its chain
            // slot already points at the block.
            let _ = install_boot_block(cpu.as_mut(), CROSS_FLAGS_ENTRY);
            let _ = install_boot_block(cpu.as_mut(), CROSS_FLAGS_ENTRY + 0x0B);
            assert_eq!(
                ready_entry(cpu.as_ref(), CROSS_FLAGS_ENTRY).insn_count,
                3,
                "precondition: the latch folded the header's loop"
            );
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
        "the folded region must leave at the interpreter's exit RIP"
    );
    assert_eq!(
        jit.gpr, iced.gpr,
        "the folded region's exit writes back every slot; a difference is a \
         missing or wrong writeback"
    );
    assert_eq!(jit.rflags, iced.rflags, "the flags leaving the loop differ");
    assert_eq!(jit.gpr[1], 0, "the loop counted RCX down to zero");
}

/// The cross-flags loop plus one block **outside** it that jumps back into the
/// loop's interior block (`+0x0B`):
///
/// ```text
/// A +0x00: sub rcx, 1        ; 48 FF C9
///   +0x03: jmp B
/// B +0x0B: jg A              ; 7F F3   (fallthrough +0x0D = ud2, the exit)
///   +0x0D: ud2
/// E +0x0F: jmp B             ; EB FA   a second backward edge, into the interior
/// ```
///
/// `E` is what makes the duplicate test possible: it is not part of A's loop, so
/// region formation would happily fold *another* region at `B` — the same guest
/// code, translated twice.
const INTERIOR_JMP: [u8; 0x11] = [
    0x48, 0xFF, 0xC9, // sub rcx, 1
    0xEB, 0x06, // jmp +0x0B
    0x90, 0x90, 0x90, 0x90, 0x90, 0x90, // padding
    0x7F, 0xF3, // jg +0x00
    0x0F, 0x0B, // ud2
    0xEB, 0xFA, // jmp +0x0B
];
const INTERIOR_JMP_ENTRY: u64 = 0x10b5_0000;

/// A loop is folded **once**: a second backward edge into the same loop must not
/// compile the same guest bytes a second time.
///
/// The case is real — DOOM Retro's hot loop was folded at both `0x140066570` and
/// its interior `0x1400665c4` before this guard, and any loop with two backward
/// branches hits it. Both copies are *correct* (that is not the risk); the risk is
/// paying a region compile per extra latch for the rest of the run.
#[test]
fn a_loop_is_folded_once_even_with_two_backward_edges_into_it() {
    let mut cpu = jit_with(INTERIOR_JMP_ENTRY, &INTERIOR_JMP, true);
    let loop_header = INTERIOR_JMP_ENTRY;
    let loop_latch = INTERIOR_JMP_ENTRY + 0x0B;
    let outside = INTERIOR_JMP_ENTRY + 0x0F;

    let _ = install_boot_block(cpu.as_mut(), loop_header);
    let _ = install_boot_block(cpu.as_mut(), loop_latch);
    assert_eq!(
        ready_entry(cpu.as_ref(), loop_header).insn_count,
        3,
        "precondition: the latch folded the loop at its header"
    );

    // `outside` branches back into the *interior* block, so formation would build a
    // second region with the same two blocks in the other order.
    let before = cpu.stats().compile.compiles;
    let block = install_boot_block(cpu.as_mut(), outside);
    assert_eq!(block.insn_count, 1, "the `jmp` block itself compiles");
    assert_eq!(
        cpu.stats().compile.compiles,
        before + 1,
        "exactly one compile — the block — means no second region was emitted; \
         a delta of 2 is the duplicate this guard exists to stop"
    );
    assert_eq!(
        ready_entry(cpu.as_ref(), loop_header).insn_count,
        3,
        "the region at the header is untouched by the later latch"
    );
}

// ── 6. the self-loop arm ────────────────────────────────────────────────

/// `add rax, 1; sub rcx, 1; jne` — a **one-block self-loop** that writes both of
/// its own live-ins.
///
/// ```text
/// +0x00: add rax, 1     ; 48 83 C0 01   reads AND writes RAX (live-in clobbered)
/// +0x04: sub rcx, 1     ; 48 FF C9      reads AND writes RCX, defines flags
/// +0x07: jne +0x00      ; 75 F7         the back edge, consuming RCX's flags
/// +0x09: ud2            ; 0F 0B          the exit target
/// ```
///
/// This is DOOM Retro's scaled-blit shape reduced to three instructions: the
/// hottest block in that guest is a self-loop, and while `backward_targets`
/// excluded `to == rip` it could never become a region and so never got the
/// once-per-entry memory hoist.
const SELF_LOOP: [u8; 0x0B] = [
    0x48, 0x83, 0xC0, 0x01, // add rax, 1
    0x48, 0xFF, 0xC9, // sub rcx, 1
    0x75, 0xF7, // jne +0x00
    0x0F, 0x0B, // ud2
];
const SELF_LOOP_ENTRY: u64 = 0x10b6_0000;
const SELF_LOOP_EXIT: u64 = SELF_LOOP_ENTRY + 0x09;

/// The tier the per-block path would pick for `rip`, which is also the only tier
/// a region that replaces it may use.
fn expected_tier(cpu: &JitCpu) -> OptTier {
    let counters = cpu.shared.tier_counters();
    assert!(
        counters.armed,
        "precondition: tiering is armed in this build"
    );
    OptTier::Speed
}

/// A self-loop is proposed as a region and folds at its **own** header, at the
/// tier the block would have had.
///
/// The shape a self-loop produces is one block either way, so `insn_count` and
/// `guest_end` cannot tell the two paths apart — the discriminating facts are that
/// a formation was attempted at all (`region_probe` records it, and the knob-off
/// arm records nothing) and that the region compiled at the block's own tier
/// rather than being downgraded to `Base`.
#[test]
fn a_self_loop_folds_at_its_own_header() {
    for region in [true, false] {
        let mut cpu = jit_with(SELF_LOOP_ENTRY, &SELF_LOOP, region);
        let block_tier = expected_tier(cpu.as_ref());
        let block = install_boot_block(cpu.as_mut(), SELF_LOOP_ENTRY);
        assert_eq!(block.insn_count, 3, "the whole loop is one block");
        assert_eq!(
            block.tier, block_tier,
            "the block path compiles a self-loop at the earned tier"
        );

        // The fold happens inside `install_boot_block`, on the same compile.
        let compiled = ready_entry(cpu.as_ref(), SELF_LOOP_ENTRY);
        assert_eq!(
            compiled.guest_start, SELF_LOOP_ENTRY,
            "keyed by its own header"
        );
        assert_eq!(
            compiled.guest_end, SELF_LOOP_EXIT,
            "and it owns the loop's bytes"
        );
        assert_eq!(
            compiled.tier, block_tier,
            "a region must never replace a `speed` block with a `base` one: the \
             tier is `decide_tier`'s answer for the same RIP, not a default"
        );
        if region {
            assert!(
                cpu.region_probe.tried.contains(&SELF_LOOP_ENTRY),
                "the latch seam must have asked formation about this header"
            );
        } else {
            assert!(
                cpu.region_probe.tried.is_empty(),
                "knob off: no formation is attempted, so the block path is \
                 untouched"
            );
        }
    }
}

/// The self-loop region, run, against the interpreter — including the property
/// that makes this fixture interesting: the loop **clobbers both of its own
/// live-ins**, so an entry that loads them once and a body that rewrites them is
/// the case where a live-in/`written` mistake would show up as a wrong answer
/// rather than a crash.
#[test]
fn a_self_loop_region_matches_the_interpreter() {
    for rcx in [1_u64, 3, 17] {
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
                    SELF_LOOP_ENTRY,
                    0x1000,
                    MEM_RESERVE | MEM_COMMIT,
                    protect::PAGE_EXECUTE_READWRITE,
                )
                .expect("map the fixture page");
                cpu.mem_write(SELF_LOOP_ENTRY, &SELF_LOOP)
                    .expect("write the fixture");
                let mut ctx = cpu.snapshot_thread_context();
                ctx.gpr[0] = 0; // RAX accumulates
                ctx.gpr[1] = rcx; // RCX counts down
                ctx.rip = SELF_LOOP_ENTRY;
                cpu.restore_thread_context(&ctx);
            }
            if let Engine::Jit(cpu) = &mut engine {
                let compiled = compile_and_install(cpu.as_mut(), SELF_LOOP_ENTRY);
                assert_eq!(
                    compiled.insn_count, 3,
                    "precondition: this RIP came out as a region, not a block"
                );
            }
            results.push(run_to_exit(&mut engine, SELF_LOOP_EXIT, SELF_LOOP_ENTRY));
        }
        let (iced, jit) = (&results[0], &results[1]);
        assert_eq!(jit.rip, iced.rip, "rcx={rcx}: exit RIP differs");
        assert_eq!(
            jit.gpr, iced.gpr,
            "rcx={rcx}: a self-loop body that rewrites its own live-ins is where \
             an entry-load/exit-writeback mistake becomes a wrong answer"
        );
        assert_eq!(jit.rflags, iced.rflags, "rcx={rcx}: flags leaving the loop");
        assert_eq!(jit.gpr[0], rcx, "RAX counted the iterations");
        assert_eq!(jit.gpr[1], 0, "RCX reached zero");
    }
}

/// A region exit must hand back the **whole** architectural word, including the
/// bits no region tracks.
///
/// The region carries RFLAGS as six SSA fields since the flag-carrier slice, and
/// one word alongside them whose only job is DF, IF, bit 1 and the reserved bits —
/// the pack at an exit clears the six tracked bits from that word and ORs the
/// fields back in. Nothing inside the region can change the untracked bits except
/// `cld`/`std`/`popfq`, which `FlagState::resync` re-seats, so a single carried
/// word is exact everywhere.
///
/// That is precisely the invariant a wrong carrier breaks *silently*: carry a zero
/// (or a fresh `iconst`) instead of the word the region was entered with and every
/// tracked flag is still right while IF and DF come back clear. The interpreter
/// preserves them, so the differential below catches it — and it is the only
/// assertion in this suite that can, because a register-only or arithmetic-only
/// check passes.
///
/// **The fixture is the two-block loop, and that is a measurement, not a taste.**
/// The same test written against the one-block self-loop (`SELF_LOOP`) does *not*
/// discriminate: with `Carry::out` mutated to carry `iconst 0`, both of its arms
/// still return `0x246` (IF|ZF|PF|bit1) on both backends. Whatever word that exit
/// writes, it is not the carried one, and no existing test notices because the six
/// tracked bits are rebuilt from the fields either way. This fixture — flags
/// written in block A, read by a `jg` in block B, one native back edge between them
/// — fails under the same mutation (`a_region_loop_matches_the_interpreter` and
/// `the_folded_region_matches_the_interpreter` both catch it), so it is the shape
/// that actually exercises the carrier.
#[test]
fn a_region_exit_keeps_the_flag_bits_the_region_never_tracked() {
    // IF (0x200) and DF (0x400) are architecturally preserved by `sub`/`jg`. Bit 1
    // is the reserved bit, always set. Starting from `Rflags::DEFAULT` means any
    // pack that rebuilds the word from the six tracked fields alone loses IF.
    const UNTRACKED: u64 = 0x400 | 0x200; // DF | IF
    for rcx in [1_u64, 4] {
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
                ctx.rflags = Rflags::DEFAULT;
                ctx.rip = CROSS_FLAGS_ENTRY;
                cpu.restore_thread_context(&ctx);
            }
            if let Engine::Jit(cpu) = &mut engine {
                let compiled = compile_and_install(cpu.as_mut(), CROSS_FLAGS_ENTRY);
                assert_eq!(
                    compiled.insn_count, 3,
                    "precondition: this RIP came out as a region, not a block"
                );
            }
            results.push(run_to_exit(
                &mut engine,
                CROSS_FLAGS_EXIT,
                CROSS_FLAGS_ENTRY,
            ));
        }
        let (iced, jit) = (&results[0], &results[1]);
        assert_eq!(
            u64::from(jit.rflags) & UNTRACKED,
            u64::from(iced.rflags) & UNTRACKED,
            "rcx={rcx}: the exit pack dropped a bit the region does not track — \
             the carrier it packs from is not the word the region was entered with"
        );
        assert_eq!(
            jit.rflags, iced.rflags,
            "rcx={rcx}: the whole exit word must match the interpreter's"
        );
    }
}

/// The self-loop plus one block **outside** it that jumps back into its entry:
///
/// ```text
/// A +0x00: add rax, 1 ; sub rcx, 1 ; jne A      (the self-loop)
///   +0x09: ud2
/// E +0x0B: jmp A                                ; EB F3
/// ```
///
/// `E` is the case where the candidate header is **not** the block being compiled,
/// so the emitter has to ask the header's own terminator whether it is a self-loop
/// rather than assume it. That answer decides the tier, and `decide_tier` memoises
/// it per VA — so a wrong `false` here does not just compile this region at the
/// base tier, it downgrades the block path's `speed` compile for the same RIP.
const SELF_LOOP_OUTSIDE_JMP: [u8; 0x0D] = [
    0x48, 0x83, 0xC0, 0x01, // add rax, 1
    0x48, 0xFF, 0xC9, // sub rcx, 1
    0x75, 0xF7, // jne +0x00
    0x0F, 0x0B, // ud2
    0xEB, 0xF3, // jmp +0x00  (a backward edge from outside the loop)
];
const SELF_LOOP_OUTSIDE_ENTRY: u64 = 0x10b8_0000;

/// A fold proposed by a **latch that is not the header** still finds the header's
/// real tier.
///
/// The latch here is `E`, whose backward successor is the self-loop's entry. If the
/// emitter assumed "not the latch ⇒ not a self-loop" it would record `Base` for that
/// VA, and the region — and the block the block path would later compile there —
/// would lose the `speed` tier that `earns_tier` promises a self-loop.
#[test]
fn a_fold_from_an_outside_latch_keeps_the_headers_earned_tier() {
    let mut cpu = jit_with(SELF_LOOP_OUTSIDE_ENTRY, &SELF_LOOP_OUTSIDE_JMP, true);
    let entry = SELF_LOOP_OUTSIDE_ENTRY;
    let latch = SELF_LOOP_OUTSIDE_ENTRY + 0x0B;
    let block_tier = expected_tier(cpu.as_ref());

    // Compile the outside block only: it is the latch, and the loop it branches
    // back into is the one-block loop at `entry`.
    let outside = install_boot_block(cpu.as_mut(), latch);
    assert_eq!(outside.insn_count, 1, "the `jmp` block itself");
    assert_eq!(
        outside.tier,
        OptTier::Base,
        "a plain `jmp` block earns no tier"
    );

    let folded = ready_entry(cpu.as_ref(), entry);
    assert_eq!(folded.insn_count, 3, "the whole self-loop, as one region");
    assert_eq!(
        folded.tier, block_tier,
        "the header's own terminator says it is a self-loop, so the region must \
         carry the tier `earns_tier` gives that block — not `Base`"
    );
}

// ── 7. self-loop shapes that must fold or refuse, never panic ───────────

/// Every self-loop shape here must come out as a region, a plain block, or a
/// refusal — never a Cranelift panic.
///
/// This exists because a self-loop is the one shape where the entry and the back
/// edge are the same guest block, and a release-only `register allocation:
/// EntryLivein` panic (a vreg live-in on the function entry, which cranelift
/// forbids) would be silent in a dev-only suite if the IR that triggers it is
/// merely *unusual* rather than obviously malformed. The region's Cranelift entry
/// block is sealed with no predecessors and every internal edge targets a state
/// block, so no shape should reach it — this test is what proves that claim over
/// the shapes a guest actually emits.
#[test]
fn self_loop_shapes_fold_or_refuse_without_panicking() {
    // `jmp $` — an unconditional one-instruction infinite loop: a one-block region
    // with **no exit at all**, so the emitted function's only exits are its
    // invalidation guard.
    let jmp_self = [0xEB_u8, 0xFE];
    // `jne $` — conditional, both a taken side and a fallthrough out.
    let jne_self = [0x75_u8, 0xFE, 0x0F, 0x0B];
    for (name, base, bytes) in [
        ("jmp $", SELF_LOOP_ENTRY + 0x2000, &jmp_self[..]),
        ("jne $", SELF_LOOP_ENTRY + 0x3000, &jne_self[..]),
    ] {
        let mut cpu = jit_with(base, bytes, true);
        // `try_compile` is the promotion entry point: whatever it decides, it must
        // return rather than abort.
        let compiled = cpu.try_compile(base).expect("a lowerable block");
        assert!(
            compiled.insn_count >= 1,
            "{name} at {base:#x} compiled to nothing"
        );
        assert_eq!(
            (compiled.guest_start, compiled.guest_end),
            (base, base + 2),
            "{name}: keyed at its own header, owning its own two bytes"
        );
    }
}

/// **The gate's wiring.** A one-block self-loop region lives behind its own knob
/// — `WIE_JIT_REGION_SELF_LOOP`, default off — and `WIE_JIT_REGION` alone must
/// not arm it. The reason it is off is the *hot* block: DOOM's scaled blit
/// `0x1800d8fb0` emits 171 host instructions per iteration as a region against
/// 156 as a block. Timed cleanly, `long_loop` is neutral within noise (median
/// ~323 ms armed vs ~330 gated, quiet host, `hops=0`, `block_entries=1`,
/// `cache_hits=1` in every arm — the back edge is native and the loop never
/// leaves the frame); the 4.5x at `4991913` and 6.3x before that commit's
/// memory-probe hoist are superseded builds.
/// `WIE_JIT_REGION_SELF_LOOP`, default off — and `WIE_JIT_REGION` alone must not
/// arm it.
///
/// This pins the latch wiring; the *decision* it feeds is pinned one layer down,
/// in `region_tests::the_self_loop_gate_is_the_only_thing_between_a_self_loop_and_a_region`,
/// where `form_region(.., false)` can be shown to return the SCC answer. Both
/// halves are needed: this one catches "the flag is read but not passed", that
/// one catches "the flag is passed but ignored".
#[test]
fn a_self_loop_region_needs_its_own_knob() {
    let base = SELF_LOOP_ENTRY + 0x6000;
    let bytes = [0x48u8, 0xFF, 0xC9, 0x75, 0xFC]; // sub rcx,1 ; jne $-2

    // `WIE_JIT_REGION` alone: regions on, self-loops off.
    let mut gated = jit_with(base, &bytes, true);
    gated
        .shared_jit()
        .region_self_loop_force
        .store(false, std::sync::atomic::Ordering::Relaxed);
    assert!(
        gated.shared_jit().region_enabled_here(),
        "precondition: the region knob is on"
    );
    assert!(
        !gated.shared_jit().region_self_loop_here(),
        "the self-loop gate must stay closed with only WIE_JIT_REGION armed"
    );
    let block = gated.try_compile(base).expect("the block still compiles");
    assert!(block.insn_count >= 1, "and it is still a lowerable block");

    // Both latches: the one-block region, which the emitter must keep correct
    // even while the policy keeps it off by default.
    let mut armed = jit_with(base, &bytes, true);
    assert!(
        armed.shared_jit().region_self_loop_here(),
        "the latch is what the pipeline reads, so it must be observable here"
    );
    let region = armed.try_compile(base).expect("the block still compiles");
    assert_eq!(
        region.insn_count, block.insn_count,
        "the same guest instructions either way — a shape decision, not a \
         coverage one"
    );
    assert!(region.func_id.is_some(), "still a real Cranelift function");
}

/// **What this pair of shapes cannot tell you**, and why that is the interesting
/// part.
///
/// A one-block region and the block path's own self-loop block are the *same*
/// emitted shape: one Cranelift function, one internal native back edge, a trip
/// charge and an invalidation guard on it (`emit::lower_self_loop_term` emits the
/// same three things the region's back edge does). So `CompiledBlock` metadata is
/// byte-for-byte identical across the knob for `jmp $`, and an assertion that
/// claimed otherwise would be a fake one.
///
/// What a one-block region adds is therefore *not* a removed hop — it is the two
/// things the region entry buys: the pin/refill descriptors hoisted **once per
/// entry** instead of probed per memory access (39.8 % of the SDL2 blit's inner
/// instructions are memory-probe guard chains), and the guest register file carried
/// as SSA block parameters instead of `JitCtx` slots. That is why the knob's frame
/// numbers were inconclusive while its compile-time numbers were not: it pays for
/// *entry* work, and a self-loop is entered once per call, not once per iteration.
#[test]
fn a_one_block_region_is_the_same_shape_as_a_self_loop_block() {
    let base = SELF_LOOP_ENTRY + 0x5000;
    let bytes = [0xEB_u8, 0xFE]; // jmp $
    let mut shapes = Vec::new();
    for region in [true, false] {
        let mut cpu = jit_with(base, &bytes, region);
        let compiled = cpu.try_compile(base).expect("a lowerable block");
        shapes.push((
            compiled.insn_count,
            compiled.guest_start,
            compiled.guest_end,
            compiled.tier,
        ));
    }
    assert_eq!(
        shapes[0], shapes[1],
        "a one-block region and a self-loop block are indistinguishable here — by          design, not by accident (see this test's doc): the block path already          emits an in-frame native back edge for a self-loop"
    );
}

/// The **no-exit** case: `jmp $`. A region whose only successor is itself can only
/// be left by the invalidation guard on its back edge, and that is exactly the
/// escape the block path's in-frame self-loop has — so a stale generation must get
/// a guest out of both arms, from the same RIP, in the same number of retired
/// instructions.
///
/// Making the guard stale *before* the run is what keeps this a finite test: the
/// compiled frame has `invalidate_gen` baked at compile time, so one dispatch runs
/// the loop's one iteration, sees the mismatch on the back edge, writes the file
/// back, points the dispatcher at the header and returns. Without that bump a
/// `jmp $` region (and a `jmp $` block) simply never returns — which is the honest
/// behaviour of an infinite guest loop and not something to assert on with a
/// timeout.
#[test]
fn a_no_exit_self_loop_is_stopped_by_the_invalidation_guard_in_both_arms() {
    let base = SELF_LOOP_ENTRY + 0x4000;
    let mut retired = Vec::new();
    let mut rips = Vec::new();
    for region in [true, false] {
        let mut cpu = jit_with(base, &[0xEB, 0xFE], region); // jmp $
        compile_and_install(cpu.as_mut(), base);
        // Stale generation: the guard this frame baked is no longer current.
        cpu.shared
            .invalidate_gen
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        let before = cpu.stats().exec.jit_insns;
        // One dispatch, so an escape that did not happen cannot spin the suite.
        cpu.run_until_stop(base, 0, 0, 1, 0, 0)
            .expect("one dispatch");
        rips.push(cpu.snapshot_thread_context().rip);
        retired.push(cpu.stats().exec.jit_insns - before);
    }
    assert_eq!(
        rips,
        vec![base, base],
        "both arms must leave the infinite loop pointed at its own header, which \
         is where the dispatcher re-enters — the trip is charged and the guest \
         makes progress rather than being silently dropped"
    );
    assert_eq!(
        retired,
        vec![2, 2],
        "and both must charge exactly the same work: the trip counter is seeded \
         with the block's static length (1 `jmp`) and bumped once by the back \
         edge, so 2 — identical in both arms, which is the parity claim"
    );
}

/// A self-loop whose body **faults** must leave the region with the guest's own
/// fault frame, exactly like a multi-block region does.
///
/// ```text
/// +0x00: inc qword [rdi]   ; 48 FF 07   the faulting access
/// +0x03: dec rcx           ; 48 FF C9
/// +0x06: jne +0x00         ; 75 F8
/// +0x08: ud2
/// ```
///
/// The fault path is the one place a one-block region emits blocks the body never
/// reaches, which is why it is worth its own run: with RDI pointing at readable
/// memory the loop counts down, and with RDI unmapped the same region must report
/// the fault at the `inc`'s IP with the register file the interpreter has.
const FAULTING_SELF_LOOP: [u8; 0x0A] = [
    0x48, 0xFF, 0x07, // inc qword [rdi]
    0x48, 0xFF, 0xC9, // dec rcx
    0x75, 0xF8, // jne +0x00
    0x0F, 0x0B, // ud2
];
const FAULTING_ENTRY: u64 = 0x10b9_0000;
const FAULTING_DATA: u64 = 0x10ba_0000;

/// The readable arm: the loop runs, counts down, and leaves at the exit.
#[test]
fn a_faulting_self_loop_runs_when_its_address_is_mapped() {
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
                FAULTING_ENTRY,
                0x1000,
                MEM_RESERVE | MEM_COMMIT,
                protect::PAGE_EXECUTE_READWRITE,
            )
            .expect("map code");
            cpu.virtual_alloc(
                FAULTING_DATA,
                0x1000,
                MEM_RESERVE | MEM_COMMIT,
                protect::PAGE_READWRITE,
            )
            .expect("map data");
            cpu.mem_write(FAULTING_ENTRY, &FAULTING_SELF_LOOP)
                .expect("write bytes");
            cpu.mem_write(FAULTING_DATA, &[0_u8; 8]).expect("zero data");
            let mut ctx = cpu.snapshot_thread_context();
            ctx.gpr[7] = FAULTING_DATA; // RDI
            ctx.gpr[1] = 3; // RCX
            ctx.rip = FAULTING_ENTRY;
            cpu.restore_thread_context(&ctx);
        }
        if let Engine::Jit(cpu) = &mut engine {
            let compiled = compile_and_install(cpu.as_mut(), FAULTING_ENTRY);
            assert_eq!(compiled.insn_count, 3, "the whole self-loop, one region");
        }
        results.push(run_to_exit(
            &mut engine,
            FAULTING_ENTRY + 0x08,
            FAULTING_ENTRY,
        ));
    }
    let (iced, jit) = (&results[0], &results[1]);
    assert_eq!(jit.rip, iced.rip, "exit RIP differs");
    assert_eq!(jit.gpr, iced.gpr, "register file differs");
    assert_eq!(jit.rflags, iced.rflags, "flags differ");
}

/// And the unmapped arm: the same region must report the fault at the guest's own
/// instruction, with the frame the interpreter has.
#[test]
fn a_fault_inside_a_self_loop_region_matches_the_interpreter() {
    let mut results: Vec<String> = Vec::new();
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
                FAULTING_ENTRY,
                0x1000,
                MEM_RESERVE | MEM_COMMIT,
                protect::PAGE_EXECUTE_READWRITE,
            )
            .expect("map code");
            cpu.mem_write(FAULTING_ENTRY, &FAULTING_SELF_LOOP)
                .expect("write bytes");
            let mut ctx = cpu.snapshot_thread_context();
            ctx.gpr[7] = 0xdead_0000; // RDI: unmapped
            ctx.gpr[1] = 3; // RCX
            ctx.rip = FAULTING_ENTRY;
            cpu.restore_thread_context(&ctx);
        }
        if let Engine::Jit(cpu) = &mut engine {
            let _ = compile_and_install(cpu.as_mut(), FAULTING_ENTRY);
        }
        let mut steps = 0_usize;
        let outcome = loop {
            steps = steps.saturating_add(1);
            assert!(steps < 4096, "fixture neither faulted nor finished");
            let before = engine.cpu().snapshot_thread_context();
            match engine.run_rmw() {
                StepResult::InvalidMemory(fault) => {
                    break format!("fault {fault:?} rip={:#x}", before.rip);
                }
                _ => {
                    let now = engine.cpu().snapshot_thread_context();
                    if now.rip == FAULTING_ENTRY + 0x08 {
                        break "completed".to_string();
                    }
                }
            }
        };
        results.push(outcome);
    }
    assert_eq!(
        results[0], results[1],
        "the faulting self-loop must leave the same guest outcome as the \
         interpreter (interpreter vs JIT)"
    );
}

/// A loop's site caches remember a page within one execution only. After a
/// re-protect, the next execution of the same compiled region must refuse the
/// store rather than serve it from what the site remembered.
#[test]
fn a_region_store_site_does_not_outlive_a_reprotect() {
    const ENTRY: u64 = 0x10ba_0000;
    const DATA: u64 = 0x10bb_0000;
    // top: test ecx,1 · jz +1 · nop · mov [rdx],rax · add rdx,8 · dec ecx · jnz top
    // then ud2.
    const LOOP: [u8; 22] = [
        0xf7, 0xc1, 0x01, 0x00, 0x00, 0x00, 0x74, 0x01, 0x90, 0x48, 0x89, 0x02, 0x48, 0x83, 0xc2,
        0x08, 0xff, 0xc9, 0x75, 0xec, 0x0f, 0x0b,
    ];
    const EXIT: u64 = ENTRY + 20;
    let mut cpu = jit_with(ENTRY, &LOOP, true);
    cpu.virtual_alloc(
        DATA,
        0x1000,
        MEM_RESERVE | MEM_COMMIT,
        protect::PAGE_READWRITE,
    )
    .expect("map the data page");
    let region = compile_and_install(cpu.as_mut(), ENTRY);
    assert!(
        region.guest_end >= EXIT,
        "precondition: one region covers the loop"
    );

    let run = |cpu: &mut JitCpu, value: u64| -> bool {
        let mut ctx = cpu.snapshot_thread_context();
        ctx.gpr[0] = value; // rax
        ctx.gpr[1] = 8; // rcx
        ctx.gpr[2] = DATA; // rdx
        ctx.rip = ENTRY;
        cpu.restore_thread_context(&ctx);
        for _ in 0..64 {
            match cpu.step_one() {
                Ok((StepResult::Continue, _)) => {}
                _ => return false,
            }
            if cpu.read_rip().expect("rip") == EXIT {
                return true;
            }
        }
        false
    };
    let read = |cpu: &mut JitCpu| {
        let mut bytes = [0_u8; 8];
        cpu.mem_read(DATA, &mut bytes).expect("read data");
        u64::from_le_bytes(bytes)
    };

    assert!(run(cpu.as_mut(), 0x11), "the writable run must finish");
    assert_eq!(read(cpu.as_mut()), 0x11);
    cpu.virtual_protect(DATA, 0x1000, protect::PAGE_READONLY)
        .expect("re-protect read-only");
    assert!(
        !run(cpu.as_mut(), 0x22),
        "a read-only store must not finish"
    );
    assert_eq!(read(cpu.as_mut()), 0x11, "and must not land");
}
