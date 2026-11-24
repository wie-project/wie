//! Tests for loop-region formation ([`super::region`]).
//!
//! Everything here runs on the real decoder over real guest bytes: each fixture
//! writes x86-64 into a [`GuestMemory`] window and asks [`form_loop_region`] what
//! it makes of it. No JIT execution is needed, because region formation is a
//! question about the control-flow graph — if a shape is discovered wrongly, the
//! emitter built on top of it would be wrong in a way no execution test of *this*
//! layer could catch.
//!
//! Two conventions in the fixtures:
//!
//! * **Exit targets are parked on `ud2`** ([`STOP`]). Exploration follows exit
//!   edges too — it has to, or "no external entry into the region" would be
//!   unevidenced — so an exit target that decoded as a lowerable block would drag
//!   the rest of the zero-filled page into the closure. `ud2` is refused by the
//!   admission gate, which makes the closure end exactly where the region does.
//! * **Jumps are never `+0`.** `decode_pure_gpr_block` merges an unconditional
//!   `jmp` whose target is its own fallthrough into the same block, so a fixture
//!   that wants a block boundary puts a byte of unreachable padding in the way
//!   (`eb 01` + one byte).

use super::super::block::BlockTerm;
use super::region::{LoopRegion, RegionLimits, RegionRefusal, form_loop_region};
use crate::RwxPerms;
use crate::mem::GuestMemory;
use iced_x86::{Mnemonic, Register};

/// Guest base of the synthetic fixtures' 64 KiB window.
const BASE: u64 = 0x3000_0000;

/// `ud2`: an instruction the operation-class gate refuses, used to end the
/// explored closure at an exit target (see the module docs).
const STOP: [u8; 2] = [0x0F, 0x0B];

fn guest() -> GuestMemory {
    let mut mem = GuestMemory::new();
    mem.map(BASE, 0x1_0000, RwxPerms::ALL)
        .expect("map the fixture window");
    mem
}

fn place(mem: &GuestMemory, va: u64, bytes: &[u8]) {
    mem.write(va, bytes).expect("write fixture bytes");
}

fn stop(mem: &GuestMemory, va: u64) {
    place(mem, va, &STOP);
}

fn form(mem: &GuestMemory, start: u64) -> Result<LoopRegion, RegionRefusal> {
    form_loop_region(mem, None, start, &RegionLimits::default())
}

fn form_with(
    mem: &GuestMemory,
    start: u64,
    limits: RegionLimits,
) -> Result<LoopRegion, RegionRefusal> {
    form_loop_region(mem, None, start, &limits)
}

/// The refusal `form` produces, panicking with the region if it produced one.
///
/// [`LoopRegion`] is not `PartialEq` (it owns decoded instructions), so a refusal
/// is asserted through this rather than through `assert_eq!` on the `Result`.
fn refusal(mem: &GuestMemory, start: u64) -> RegionRefusal {
    match form(mem, start) {
        Ok(region) => panic!("expected a refusal at {start:#x}, got {region:?}"),
        Err(why) => why,
    }
}

fn refusal_with(mem: &GuestMemory, start: u64, limits: RegionLimits) -> RegionRefusal {
    match form_with(mem, start, limits) {
        Ok(region) => panic!("expected a refusal at {start:#x}, got {region:?}"),
        Err(why) => why,
    }
}

fn gprs(region: &LoopRegion) -> Vec<Register> {
    region.live_in().gprs().collect()
}

fn starts(region: &LoopRegion) -> Vec<u64> {
    region.blocks().iter().map(|block| block.start()).collect()
}

/// DOOM Retro's hot loop, in miniature: two blocks, the second one containing the
/// conditional back edge to the first.
///
/// ```text
/// A BASE+0x00: movzx eax, byte [rdi]      ; reads RDI
///   BASE+0x03: mov   r9d, r14d            ; reads R14
///   BASE+0x06: sub   r9d, eax             ; RAX already defined here
///   BASE+0x09: mov   ecx, r15d            ; reads R15
///   BASE+0x0c: jmp   BASE+0x0f            ; (one byte of padding at 0x0e)
/// B BASE+0x0f: cmp   ecx, ebx             ; reads EBX, RCX
///   BASE+0x11: cmovge edx, esi            ; reads EDX, ESI, flags
///   BASE+0x14: inc   r11d                 ; reads R11
///   BASE+0x17: cmp   r11d, 0x100
///   BASE+0x1e: jl    BASE                 ; back edge
///     exit: BASE+0x20
/// ```
const DOOM_SHAPE: [u8; 0x20] = [
    0x0F, 0xB6, 0x07, // movzx eax, byte [rdi]
    0x45, 0x8B, 0xCE, // mov r9d, r14d
    0x44, 0x2B, 0xC8, // sub r9d, eax
    0x41, 0x8B, 0xCF, // mov ecx, r15d
    0xEB, 0x01, // jmp BASE+0x0f
    0x90, // unreachable padding
    0x3B, 0xCB, // cmp ecx, ebx
    0x0F, 0x4D, 0xD6, // cmovge edx, esi
    0x41, 0xFF, 0xC3, // inc r11d
    0x41, 0x81, 0xFB, 0x00, 0x01, 0x00, 0x00, // cmp r11d, 0x100
    0x7C, 0xE0, // jl BASE
];

#[test]
fn doom_shaped_two_block_loop_has_the_right_live_ins() {
    let mem = guest();
    place(&mem, BASE, &DOOM_SHAPE);
    stop(&mem, BASE + 0x20);

    let region = form(&mem, BASE).expect("a 2-block jl loop is a region");

    assert_eq!(region.entry(), BASE);
    assert_eq!(
        starts(&region),
        vec![BASE, BASE + 0x0F],
        "the two blocks, entry first, at the VAs the guest jumps between"
    );
    assert_eq!(region.insn_count(), 10, "5 + 5 guest instructions");
    assert_eq!(region.blocks()[0].body_len(), 4, "jmp is the terminator");
    assert_eq!(region.blocks()[1].body_len(), 4, "jl is the terminator");
    assert_eq!(
        region.back_edges(),
        &[(1, 0)],
        "the second block's jl is the back edge to the entry"
    );

    // Live-ins: what the region reads before writing on some path from the entry.
    // RAX and RCX are *not* here — each is written before it is read — while R9
    // would be a live-in only if `sub r9d, eax` came before `mov r9d, r14d`.
    assert_eq!(
        gprs(&region),
        vec![
            Register::RDX,
            Register::RBX,
            Register::RSI,
            Register::RDI,
            Register::R11,
            Register::R14,
            Register::R15,
        ],
        "read after being written is not a live-in; read before is"
    );
    assert!(!region.live_in().contains_gpr(Register::RAX));
    assert!(!region.live_in().contains_gpr(Register::RCX));
    assert!(
        !region.live_in().rflags(),
        "the jl's flags are produced inside"
    );

    // One exit: the jl's not-taken side.
    assert_eq!(
        region.exits().len(),
        1,
        "a jcc contributes the side that leaves: {:?}",
        region.exits()
    );
    let exit = region.exits()[0];
    assert_eq!(exit.target(), BASE + 0x20);
    assert_eq!(exit.from(), 1);
    assert!(!exit.is_taken(), "a fallthrough, not the branch target");
    assert_eq!(region.exit_targets(), vec![BASE + 0x20]);
}

/// A diamond *inside* a loop, with the side exit the diamond's join block takes.
///
/// ```text
/// H BASE+0x100: cmp eax, 0
///   BASE+0x103: jz  BASE+0x109        ; T and E are the two arms
/// T BASE+0x105: inc ecx
///   BASE+0x107: jmp BASE+0x10e
/// E BASE+0x109: dec ecx
///   BASE+0x10b: jmp BASE+0x10e
/// J BASE+0x10e: cmp ecx, 10
///   BASE+0x111: jne BASE              ; back edge
///     exit: BASE+0x113                ; side exit
/// ```
fn diamond() -> GuestMemory {
    let mem = guest();
    place(
        &mem,
        BASE + 0x100,
        &[
            0x83, 0xF8, 0x00, // cmp eax, 0
            0x74, 0x04, // jz BASE+0x109
            0xFF, 0xC1, // inc ecx
            0xEB, 0x05, // jmp BASE+0x10e
            0xFF, 0xC9, // dec ecx
            0xEB, 0x01, // jmp BASE+0x10e
            0x90, // unreachable padding
            0x83, 0xF9, 0x0A, // cmp ecx, 10
            0x75, 0xED, // jne BASE
        ],
    );
    stop(&mem, BASE + 0x113);
    mem
}

#[test]
fn diamond_with_a_side_exit_has_the_right_exit_set() {
    let mem = diamond();
    let region = form(&mem, BASE + 0x100).expect("a diamond loop is a region");

    let mut blocks = starts(&region);
    assert_eq!(blocks.remove(0), BASE + 0x100, "the entry orders first");
    blocks.sort_unstable();
    assert_eq!(
        blocks,
        vec![BASE + 0x105, BASE + 0x109, BASE + 0x10E],
        "both arms and the join are region blocks"
    );
    // Four blocks of two instructions (the terminator counts): derive the total
    // from the blocks rather than restating it, so a fixture change that alters
    // one block's length fails on the block, not on a magic total.
    assert!(
        region.blocks().iter().all(|block| block.insn_count() == 2),
        "every diamond block is one instruction plus its terminator: {:?}",
        region
            .blocks()
            .iter()
            .map(|block| (block.start(), block.insn_count()))
            .collect::<Vec<_>>()
    );
    assert_eq!(region.insn_count(), 8);

    // The only cycle goes through the entry: the two arms are forward edges.
    assert_eq!(region.back_edges().len(), 1);
    assert_eq!(
        region.back_edges()[0].1,
        0,
        "the back edge returns to the entry"
    );

    // The exit set is the jne's not-taken side and nothing else: the jz and both
    // jmps stay inside.
    assert_eq!(region.exit_targets(), vec![BASE + 0x113]);
    let exit = region.exits()[0];
    assert_eq!(exit.from(), 3, "the join block is region block 3");
    assert!(!exit.is_taken());

    // ECX is written in both arms and read before that write on the path through
    // the entry — the must-analysis has to keep it, and EAX is read by the
    // header. This is the case a `reads \ writes` up-set gets wrong.
    assert_eq!(gprs(&region), vec![Register::RAX, Register::RCX]);
    assert!(
        !region.live_in().rflags(),
        "cmp in the header defines flags"
    );
}

/// The textbook irreducible CFG: two entries into one cycle, no dominant header.
///
/// ```text
/// A BASE+0x200: je  B
/// X BASE+0x202: jmp C               ; A's other side reaches C
/// B BASE+0x206: jmp D               ; entry 1 of {B, C, D, F}
/// C BASE+0x20a: jmp D               ; entry 2
/// D BASE+0x20e: jne B
/// F BASE+0x210: jmp C
/// ```
///
/// Asked about `B`, the region is the whole cycle `{B, C, D, F}`, and it contains
/// `C → D → F → C`, a cycle that never returns to the entry: no single header, so
/// no single set of entry live-ins.
#[test]
fn an_irreducible_cfg_is_refused() {
    let mem = guest();
    place(
        &mem,
        BASE + 0x200,
        &[
            0x74, 0x04, // je BASE+0x206
            0xEB, 0x06, // jmp BASE+0x20a
            0x90, 0x90, // padding
            0xEB, 0x06, // jmp BASE+0x20e
            0x90, 0x90, // padding
            0xEB, 0x02, // jmp BASE+0x20e
            0x90, 0x90, // padding
            0x75, 0xF6, // jne BASE+0x206
            0xEB, 0xF8, // jmp BASE+0x20a
        ],
    );

    assert_eq!(
        refusal(&mem, BASE + 0x206),
        RegionRefusal::Irreducible { at: BASE + 0x20E },
        "a cycle through D that never returns to B is the irreducible signature"
    );
}

/// A ring of `N` two-instruction blocks: `nop; jmp <next>`, the last one jumping
/// back to the first. The simplest possible loop with a dialable size.
fn ring(n: usize) -> GuestMemory {
    let mem = guest();
    for i in 0..n {
        let at = BASE + 0x300 + (i as u64) * 8;
        let next = if i + 1 == n { BASE + 0x300 } else { at + 8 };
        // `nop` then `jmp` targeting the next block's start: displacement is
        // measured from the end of the jmp, so it is `next - (at + 3)`.
        let disp = i8::try_from(next.cast_signed() - (at.cast_signed() + 3))
            .expect("ring block must be a rel8 jump away");
        place(&mem, at, &[0x90, 0xEB, disp.cast_unsigned()]);
    }
    mem
}

#[test]
fn a_ring_forms_a_region_and_the_bounds_are_enforced() {
    let mem = ring(5);
    let region = form(&mem, BASE + 0x300).expect("a 5-block ring is a region");
    assert_eq!(region.blocks().len(), 5);
    assert_eq!(region.insn_count(), 10);
    assert_eq!(region.back_edges(), &[(4, 0)]);
    assert!(region.exits().is_empty(), "a closed ring leaves nowhere");

    assert_eq!(
        refusal_with(
            &mem,
            BASE + 0x300,
            RegionLimits {
                max_blocks: 4,
                ..RegionLimits::default()
            }
        ),
        RegionRefusal::TooManyBlocks {
            blocks: 5,
            limit: 4
        }
    );

    assert_eq!(
        refusal_with(
            &mem,
            BASE + 0x300,
            RegionLimits {
                max_insns: 9,
                ..RegionLimits::default()
            }
        ),
        RegionRefusal::TooManyInsns {
            insns: 10,
            limit: 9
        }
    );

    assert_eq!(
        refusal_with(
            &mem,
            BASE + 0x300,
            RegionLimits {
                max_explore_blocks: 3,
                ..RegionLimits::default()
            }
        ),
        RegionRefusal::ExplorationTooLarge,
        "exploration refuses before it can claim a region it did not see whole"
    );
}

/// A loop with two distinct exits, to size the exit-target bound.
///
/// ```text
/// A BASE+0x500: jne BASE+0x508       ; not-taken BASE+0x502 is an exit
///   BASE+0x502: ud2                  ; exit 1
/// B BASE+0x508: je  BASE+0x510       ; taken BASE+0x510 is an exit
///   BASE+0x50a: jmp BASE             ; region block 2
///   BASE+0x510: ud2                  ; exit 2
/// ```
#[test]
fn the_exit_target_bound_is_enforced() {
    let mem = guest();
    place(
        &mem,
        BASE + 0x500,
        &[
            0x75, 0x06, // jne BASE+0x508
            0x0F, 0x0B, // ud2 (exit 1)
            0x90, 0x90, 0x90, 0x90, // padding
            0x74, 0x06, // je BASE+0x510
            0xEB, 0xF4, // jmp BASE
            0x90, 0x90, 0x90, 0x90, // padding
            0x0F, 0x0B, // ud2 (exit 2)
        ],
    );

    let region = form(&mem, BASE + 0x500).expect("a 3-block loop with two exits");
    assert_eq!(region.blocks().len(), 3);
    assert_eq!(region.exit_targets(), vec![BASE + 0x502, BASE + 0x510]);
    assert_eq!(region.exits().len(), 2);

    assert_eq!(
        refusal_with(
            &mem,
            BASE + 0x500,
            RegionLimits {
                max_exit_targets: 1,
                ..RegionLimits::default()
            }
        ),
        RegionRefusal::TooManyExitTargets {
            targets: 2,
            limit: 1
        }
    );
}

/// A call inside the cycle: `inc ecx; call rel32`, then a jump back.
///
/// The call is explored *through its return address*, which is what makes this a
/// cycle at all — the region is then refused by name instead of being reported as
/// "no back edge", which is what a discovery that ignored the return edge would
/// say and what would hide a real call from the reader of the refusal.
#[test]
fn a_region_containing_a_call_is_refused() {
    let mem = guest();
    place(
        &mem,
        BASE + 0x600,
        &[
            0xFF, 0xC1, // inc ecx
            0xE8, 0xFB, 0x00, 0x00, 0x00, // call BASE+0x700
            0xEB, 0xF7, // jmp BASE
        ],
    );

    assert_eq!(
        refusal(&mem, BASE + 0x600),
        RegionRefusal::Call { at: BASE + 0x602 }
    );
}

/// `movsb` then a jump back: a string op inside a loop.
#[test]
fn a_region_containing_a_string_op_is_refused() {
    let mem = guest();
    place(
        &mem,
        BASE + 0x680,
        &[
            0xA4, // movsb
            0xEB, 0xFD, // jmp BASE+0x680
        ],
    );

    assert_eq!(
        refusal(&mem, BASE + 0x680),
        RegionRefusal::StringOp { at: BASE + 0x680 }
    );
}

/// `mov eax, [ebx]` — an address-size override, whose effective address the
/// emitter cannot reproduce (it adds the full 64-bit `EBX`/`RBX` value), so the
/// refill probe would be handed an address the guest never computed.
#[test]
fn a_region_with_an_unguardable_register_operand_is_refused() {
    let mem = guest();
    place(
        &mem,
        BASE + 0x700,
        &[
            0x67, 0x8B, 0x03, // mov eax, dword ptr [ebx]  (addr32)
            0xEB, 0x01, // jmp BASE+0x706
            0x90, // unreachable padding
            0xEB, 0xF8, // jmp BASE+0x700
        ],
    );

    assert_eq!(
        refusal(&mem, BASE + 0x700),
        RegionRefusal::UnguardableAddress {
            at: BASE + 0x700,
            why: "addr32"
        }
    );
}

/// The two ways there is no region: the start is not on a cycle, or not lowerable
/// at all.
#[test]
fn blocks_off_a_cycle_or_off_the_gate_have_no_region() {
    let mem = guest();
    place(
        &mem,
        BASE + 0x800,
        &[
            0xFF, 0xC1, // inc ecx
            0xEB, 0x01, // jmp BASE+0x803  (forward, not a cycle)
            0x90, // padding
            0xFF, 0xC9, // dec ecx
            0xEB, 0x01, // jmp BASE+0x807
            0x90, // padding
            0x0F, 0x0B, // ud2
        ],
    );

    assert_eq!(
        refusal(&mem, BASE + 0x800),
        RegionRefusal::NoBackEdge,
        "a straight-line chain is not a loop"
    );
    assert_eq!(
        refusal(&mem, BASE + 0x80A),
        RegionRefusal::EntryNotLowerable,
        "a block the JIT will not compile cannot anchor a region"
    );
}

/// Every edge a region block has either stays inside the region or is a recorded
/// exit — the closure property the emitter will rely on to keep guest state in SSA
/// values across a back edge.
#[test]
fn every_region_edge_stays_inside_or_is_an_exit() {
    let cases: [(&str, GuestMemory, u64); 3] = [
        (
            "doom shape",
            {
                let mem = guest();
                place(&mem, BASE, &DOOM_SHAPE);
                stop(&mem, BASE + 0x20);
                mem
            },
            BASE,
        ),
        ("diamond", diamond(), BASE + 0x100),
        ("ring", ring(3), BASE + 0x300),
    ];
    for (name, mem, entry) in cases {
        let region = form(&mem, entry).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(
            region.blocks()[0].start(),
            region.entry(),
            "{name}: the entry orders first"
        );
        let mut starts: Vec<u64> = starts(&region);
        starts.sort_unstable();
        for (index, block) in region.blocks().iter().enumerate() {
            let (slots, len) = block.successors();
            for &target in &slots[..len] {
                let inside = starts.binary_search(&target).is_ok();
                let recorded = region
                    .exits()
                    .iter()
                    .any(|exit| exit.from() == index && exit.target() == target);
                assert!(
                    inside || recorded,
                    "{name}: block {} -> {target:#x} is neither inside nor an exit",
                    block.start()
                );
            }
        }
        for exit in region.exits() {
            assert!(
                starts.binary_search(&exit.target()).is_err(),
                "{name}: exit target {:#x} must be outside the region",
                exit.target()
            );
        }
    }
}

/// The fixture that matters: DOOM Retro's actual hot loop.
///
/// `real_exes/doomretro/doomretro.exe` is not in every checkout, so the test skips
/// cleanly when it is absent — and says so, rather than passing silently as if it
/// had checked something.
#[test]
fn doomretro_hot_loop_is_discovered() {
    let Some(image) = PeImage::open() else {
        eprintln!("skip: real_exes/doomretro/doomretro.exe not present");
        return;
    };
    let (entry, second) = (0x1_4006_6570, 0x1_4006_65C4);
    let mem = image.guest_window(0x1_4006_6000, 0x4000);

    let region = form(&mem, entry).expect("DOOM's hot loop forms a region");
    // findings printed for the record; the assertions below are the pinned part
    eprintln!(
        "doom region: entry={:#x} blocks={:?} insns={} live_in={:?} exits={:#x?} back={:?}",
        region.entry(),
        starts(&region),
        region.insn_count(),
        gprs(&region),
        region.exit_targets(),
        region.back_edges()
    );

    // How wide the closure is: the smallest `max_explore_blocks` that still forms
    // the region. Worth watching rather than asserting tighter — it is the number
    // that says how much of DOOM Retro this discovery had to walk to be sure the
    // loop had no other entry.
    let mut lo = 1usize;
    let mut hi = RegionLimits::default().max_explore_blocks;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let fits = form_with(
            &mem,
            entry,
            RegionLimits {
                max_explore_blocks: mid,
                ..RegionLimits::default()
            },
        )
        .is_ok();
        if fits {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    eprintln!("doom: explored closure = {lo} blocks");

    assert_eq!(region.entry(), entry);
    assert_eq!(
        starts(&region),
        vec![entry, second],
        "DOOM's loop is exactly these two blocks, entry first"
    );
    assert_eq!(region.insn_count(), 36, "25 + 11 guest instructions");
    assert_eq!(
        region.blocks()[0].insn_count(),
        25,
        "DOOM's first block is 25 instructions"
    );
    assert_eq!(region.blocks()[1].insn_count(), 11, "and the second is 11");
    assert_eq!(region.insns().len(), 36);
    assert_eq!(region.insns()[0].rip, entry, "the first insn is the entry");
    assert_eq!(
        region.blocks()[0].body_len(),
        24,
        "je is block 0's terminator"
    );
    assert_eq!(
        region.blocks()[1].body_len(),
        10,
        "jl is block 1's terminator"
    );
    assert_eq!(
        region.back_edges(),
        &[(1, 0)],
        "block 1's jl is the back edge"
    );
    // Block 0 ends in a `je`: the taken side is the epilogue exit, the
    // fallthrough is block 1. That is the shape that makes this loop a loop and
    // a single-side exit at the same time.
    assert!(
        matches!(
            region.blocks()[0].term(),
            Some(BlockTerm::Jcc {
                mnemonic: Mnemonic::Je,
                taken,
                not_taken,
            }) if taken == 0x1_4006_65F0 && not_taken == second
        ),
        "block 0 is `je <epilogue>`, falling through to block 1: {:?}",
        region.blocks()[0].term()
    );
    assert_eq!(region.blocks()[0].end_rip(), second);
    assert_eq!(region.blocks()[1].end_rip(), 0x1_4006_65E5);

    // Both blocks exit: block 0 through its je's *taken* side, block 1 through
    // its jl's *not-taken* side. So the loop has two exits, one per block, and a
    // region emitter has to emit both.
    let exits = region.exits();
    assert_eq!(
        exits
            .iter()
            .map(|exit| (exit.from(), exit.target(), exit.is_taken()))
            .collect::<Vec<_>>(),
        vec![
            (0, 0x1_4006_65F0, true),  // block 0's je, taken
            (1, 0x1_4006_65E5, false), // block 1's jl, fallthrough
        ]
    );
    assert_eq!(region.exit_targets(), vec![0x1_4006_65F0, 0x1_4006_65E5]);

    // What the loop reads before writing. No XMM: DOOM Retro's inner loop is
    // scalar integer and LEA only (three `movzx` bytes off RDI plus fixed-point
    // shifts and multiplies) — so a region here needs seven GPR loads at entry and
    // no XMM traffic at all.
    assert_eq!(
        gprs(&region),
        vec![
            Register::RBX,
            Register::RBP,
            Register::RSI,
            Register::RDI,
            Register::R11,
            Register::R14,
            Register::R15,
        ]
    );
    assert_eq!(region.live_in().xmms().count(), 0);
    assert_eq!(region.live_in().reg_count(), 7);
    assert!(!region.live_in().contains_xmm(0));
    assert!(
        !region.live_in().rflags(),
        "the `test` in block 0 defines flags"
    );
    // The loop's memory operands are dynamic (`[rdi]`, `[rdi+1]`, `[rdi+2]`,
    // `lea r8d, [r14+rax]`, …) and the refill probe guards them: this is the
    // positive control for rule 3, which refuses only addresses the emitter
    // cannot reproduce.
    assert!(!region.live_in().contains_gpr(Register::RSP));
}

/// A minimal read of the PE image: enough to place section bytes at the guest VAs
/// they are mapped at, without taking `wie-pe` as a dependency for one fixture.
struct PeImage {
    file: Vec<u8>,
    image_base: u64,
    /// `(virtual_address, virtual_size, raw_ptr, raw_size)` per section.
    sections: Vec<(u32, u32, u32, u32)>,
}

impl PeImage {
    fn open() -> Option<Self> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../real_exes/doomretro/doomretro.exe");
        let file = std::fs::read(&path).ok()?;
        let lfanew = u32::from_le_bytes(file.get(0x3C..0x40)?.try_into().ok()?) as usize;
        let coff = lfanew.checked_add(4)?;
        if file.get(lfanew..coff)? != b"PE\0\0" {
            return None;
        }
        let nsec = u16::from_le_bytes(file.get(coff + 2..coff + 4)?.try_into().ok()?);
        let size_opt = u16::from_le_bytes(file.get(coff + 16..coff + 18)?.try_into().ok()?);
        let opt = coff + 20;
        let magic = u16::from_le_bytes(file.get(opt..opt + 2)?.try_into().ok()?);
        if magic != 0x20B {
            return None;
        }
        let image_base = u64::from_le_bytes(file.get(opt + 24..opt + 32)?.try_into().ok()?);
        let sh = opt + usize::from(size_opt);
        let mut sections = Vec::with_capacity(usize::from(nsec));
        for i in 0..usize::from(nsec) {
            let at = sh + i * 40;
            let vsize = u32::from_le_bytes(file.get(at + 8..at + 12)?.try_into().ok()?);
            let vaddr = u32::from_le_bytes(file.get(at + 12..at + 16)?.try_into().ok()?);
            let raw_size = u32::from_le_bytes(file.get(at + 16..at + 20)?.try_into().ok()?);
            let raw_ptr = u32::from_le_bytes(file.get(at + 20..at + 24)?.try_into().ok()?);
            sections.push((vaddr, vsize, raw_ptr, raw_size));
        }
        Some(Self {
            file,
            image_base,
            sections,
        })
    }

    /// Guest memory holding `[base, base + size)`, filled from the image.
    fn guest_window(&self, base: u64, size: usize) -> GuestMemory {
        let mut mem = GuestMemory::new();
        mem.map(base, size, RwxPerms::ALL)
            .expect("map the PE window");
        let mut bytes = vec![0_u8; size];
        let end = base + size as u64;
        for (vaddr, vsize, raw_ptr, raw_size) in &self.sections {
            let sec_start = self.image_base + u64::from(*vaddr);
            // A section occupies `max(virtual_size, raw_size)` guest bytes, but
            // only its first `raw_size` bytes exist in the file; the rest is zero
            // fill, which `bytes` already is.
            let sec_span = u64::from((*vsize).max(*raw_size));
            let lo = sec_start.max(base);
            let hi = (sec_start + sec_span).min(end);
            let Some(len) = hi.checked_sub(lo).and_then(|len| usize::try_from(len).ok()) else {
                continue;
            };
            if len == 0 {
                continue;
            }
            let in_section = usize::try_from(lo - sec_start).unwrap_or(usize::MAX);
            let raw_left = usize::try_from(*raw_size)
                .unwrap_or(0)
                .saturating_sub(in_section);
            let file_at = usize::try_from(*raw_ptr)
                .ok()
                .map(|ptr| ptr.saturating_add(in_section));
            let dst_at = usize::try_from(lo - base).unwrap_or(usize::MAX);
            let (Some(file_at), Some(dst)) = (file_at, bytes.get_mut(dst_at..)) else {
                continue;
            };
            let Some(src) = self.file.get(file_at..) else {
                continue;
            };
            let n = len.min(raw_left).min(src.len()).min(dst.len());
            dst[..n].copy_from_slice(&src[..n]);
        }
        mem.write(base, &bytes).expect("write the PE window");
        mem
    }
}
