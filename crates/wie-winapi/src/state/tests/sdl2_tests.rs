//! SDL2 host-handler tests: a guest `SDL_Surface` driven through
//! fill / blit / lock / unlock, with the result compared byte for byte.
//!
//! These are the tests that decide whether the host may keep the fast path.
//! Every case here is set up the way the guest's own structures are laid out
//! (see `crate::sdl2::surface` for the offsets and how they were verified), and
//! each one asserts on the *bytes in guest memory* after the call — not on the
//! handler's return value alone, because a blit that returns 0 and writes the
//! wrong pixels is the failure mode that matters.
//!
//! The fall-through test is the other half of the contract: when a handler
//! declines (here: a blend-mode blit), it must leave the guest's argument
//! registers and stack alone and redirect RIP to the module's own code.

use super::*;

// ── Layout of the fake guest surface graph ─────────────────────────────────
const SRC_SURFACE: u64 = 0x2000;
const SRC_FORMAT: u64 = 0x2100;
const DST_SURFACE: u64 = 0x2200;
const DST_FORMAT: u64 = 0x2300;
const BLIT_MAP: u64 = 0x2400;
const PALETTE: u64 = 0x2500;
const LUT: u64 = 0x2600;
const SRC_PIXELS: u64 = 0x3000;
const DST_PIXELS: u64 = 0x4000;
const RECT: u64 = 0x5000;

/// `SDL_PIXELFORMAT_INDEX8`.
const PIXELFORMAT_INDEX8: u32 = 0x1300_0000;
/// `SDL_PIXELFORMAT_ARGB8888`.
const PIXELFORMAT_ARGB8888: u32 = 0x1636_2004;

/// Write a guest `SDL_PixelFormat`.
fn write_format(engine: &mut IcedCpu, va: u64, format: u32, palette: u64, bpp: u8, bytes_pp: u8) {
    let mut bytes = [0_u8; crate::sdl2::surface::FORMAT_SIZE];
    bytes[0..4].copy_from_slice(&format.to_le_bytes());
    bytes[8..16].copy_from_slice(&palette.to_le_bytes());
    bytes[16] = bpp;
    bytes[17] = bytes_pp;
    engine.mem_write(va, &bytes).expect("write pixel format");
}

/// Write a guest `SDL_Surface`.
///
/// One argument per field the handlers read: a builder struct would restate
/// them, and the fixtures read better when the call matches the layout.
#[expect(
    clippy::too_many_arguments,
    reason = "one argument per SDL_Surface field the handlers read"
)]
fn write_surface(
    engine: &mut IcedCpu,
    va: u64,
    flags: u32,
    format: u64,
    w: i32,
    h: i32,
    pitch: i32,
    pixels: u64,
    locked: i32,
    clip: (i16, i16, u16, u16),
    map: u64,
) {
    let mut bytes = [0_u8; crate::sdl2::surface::SURFACE_SIZE];
    bytes[0..4].copy_from_slice(&flags.to_le_bytes());
    bytes[8..16].copy_from_slice(&format.to_le_bytes());
    bytes[16..20].copy_from_slice(&w.to_le_bytes());
    bytes[20..24].copy_from_slice(&h.to_le_bytes());
    bytes[24..28].copy_from_slice(&pitch.to_le_bytes());
    bytes[32..40].copy_from_slice(&pixels.to_le_bytes());
    bytes[48..52].copy_from_slice(&locked.to_le_bytes());
    bytes[64..66].copy_from_slice(&clip.0.to_le_bytes());
    bytes[66..68].copy_from_slice(&clip.1.to_le_bytes());
    bytes[68..70].copy_from_slice(&clip.2.to_le_bytes());
    bytes[70..72].copy_from_slice(&clip.3.to_le_bytes());
    bytes[80..88].copy_from_slice(&map.to_le_bytes());
    engine.mem_write(va, &bytes).expect("write surface");
}

/// Write the `SDL_BlitMap` fields a blit decision reads.
///
/// `src_fmt` / `dst_fmt` / `src_pitch` / `dst_pitch` are the mirrors
/// `SDL_CalculateBlit` maintains; the handler rejects the mapping when they
/// disagree with the surfaces, so the fixtures must fill them consistently.
#[expect(
    clippy::too_many_arguments,
    reason = "one argument per SDL_BlitMap field the handlers read"
)]
fn write_blit_map(
    engine: &mut IcedCpu,
    va: u64,
    dst: u64,
    identity: i32,
    src_fmt: u64,
    dst_fmt: u64,
    src_pitch: i32,
    dst_pitch: i32,
    table: u64,
    flags: i32,
    colorkey: u32,
    palette_versions: (u32, u32),
) {
    let mut bytes = [0_u8; crate::sdl2::surface::MAP_SIZE];
    bytes[0..8].copy_from_slice(&dst.to_le_bytes());
    bytes[8..12].copy_from_slice(&identity.to_le_bytes());
    let info = 32_usize;
    bytes[info + 16..info + 20].copy_from_slice(&src_pitch.to_le_bytes());
    bytes[info + 40..info + 44].copy_from_slice(&dst_pitch.to_le_bytes());
    bytes[info + 48..info + 56].copy_from_slice(&src_fmt.to_le_bytes());
    bytes[info + 56..info + 64].copy_from_slice(&dst_fmt.to_le_bytes());
    bytes[info + 64..info + 72].copy_from_slice(&table.to_le_bytes());
    bytes[info + 72..info + 76].copy_from_slice(&flags.to_le_bytes());
    bytes[info + 76..info + 80].copy_from_slice(&colorkey.to_le_bytes());
    bytes[120..124].copy_from_slice(&palette_versions.0.to_le_bytes());
    bytes[124..128].copy_from_slice(&palette_versions.1.to_le_bytes());
    engine.mem_write(va, &bytes).expect("write blit map");
}

/// Write a guest `SDL_Rect`.
fn write_guest_rect(engine: &mut IcedCpu, va: u64, x: i16, y: i16, w: u16, h: u16) {
    let mut bytes = [0_u8; 8];
    bytes[0..2].copy_from_slice(&x.to_le_bytes());
    bytes[2..4].copy_from_slice(&y.to_le_bytes());
    bytes[4..6].copy_from_slice(&w.to_le_bytes());
    bytes[6..8].copy_from_slice(&h.to_le_bytes());
    engine.mem_write(va, &bytes).expect("write rect");
}

fn read_bytes(engine: &mut IcedCpu, va: u64, len: usize) -> Vec<u8> {
    let mut bytes = vec![0_u8; len];
    engine.mem_read(va, &mut bytes).expect("read guest bytes");
    bytes
}

/// A 4x3 8bpp source surface and a 4x3 32bpp destination, sharing a blit map
/// built for a palette-expanding (non-identity) mapping, with a 256-entry table
/// where entry `i` is `0x0100_0000 + i`.
fn setup_expanding_blit(engine: &mut IcedCpu) {
    write_format(engine, SRC_FORMAT, PIXELFORMAT_INDEX8, PALETTE, 8, 1);
    write_format(engine, DST_FORMAT, PIXELFORMAT_ARGB8888, 0, 32, 4);
    write_blit_map(
        engine,
        BLIT_MAP,
        DST_SURFACE,
        0,
        SRC_FORMAT,
        DST_FORMAT,
        4,
        16,
        LUT,
        0,
        0,
        (1, 1),
    );
    // 4-wide, 3-tall 8bpp source (pitch 4) and 32bpp destination (pitch 16).
    write_surface(
        engine,
        SRC_SURFACE,
        0,
        SRC_FORMAT,
        4,
        3,
        4,
        SRC_PIXELS,
        0,
        (0, 0, 4, 3),
        BLIT_MAP,
    );
    write_surface(
        engine,
        DST_SURFACE,
        0,
        DST_FORMAT,
        4,
        3,
        16,
        DST_PIXELS,
        0,
        (0, 0, 4, 3),
        0,
    );
    // Palette version 1 on both the palette object and the mapping.
    let mut palette = [0_u8; crate::sdl2::surface::PALETTE_SIZE];
    palette[0..4].copy_from_slice(&256_i32.to_le_bytes());
    palette[16..20].copy_from_slice(&1_u32.to_le_bytes());
    engine.mem_write(PALETTE, &palette).expect("write palette");
    // Table: entry i = 0x0100_0000 + i.
    let mut table = [0_u8; 1024];
    for i in 0..256_usize {
        let value = 0x0100_0000_u32 + u32::try_from(i).unwrap_or(0);
        table[i * 4..i * 4 + 4].copy_from_slice(&value.to_le_bytes());
    }
    engine.mem_write(LUT, &table).expect("write lut");
    // Source pixels 0,1,2,3 / 4,5,6,7 / 8,9,10,11.
    let src: Vec<u8> = (0_u8..12).collect();
    engine
        .mem_write(SRC_PIXELS, &src)
        .expect("write src pixels");
    // Destination starts as a recognisable pattern so untouched bytes show.
    engine
        .mem_write(DST_PIXELS, &[0xEE_u8; 48])
        .expect("write dst pixels");
}

/// Palette expansion through the guest's own table, writing into the
/// destination surface the presenter would read.
#[test]
fn blit_expands_palette_straight_into_the_destination() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    setup_expanding_blit(&mut engine);

    write_guest_rect(&mut engine, RECT, 0, 0, 4, 3);
    let dst_rect = 0x5010_u64;
    write_guest_rect(&mut engine, dst_rect, 0, 0, 4, 3);
    write_regs(&mut engine, SRC_SURFACE, RECT, DST_SURFACE, dst_rect, 0);
    assert_return_value!(
        crate::sdl2::blit::handle_lower_blit(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );

    let out = read_bytes(&mut engine, DST_PIXELS, 48);
    for row in 0..3_usize {
        for column in 0..4_usize {
            let index = u32::try_from(row * 4 + column).unwrap_or(0);
            let expected = (0x0100_0000_u32 + index).to_le_bytes();
            let at = row * 16 + column * 4;
            assert_eq!(
                &out[at..at + 4],
                &expected,
                "row {row} column {column} must be the table entry for index {index}"
            );
        }
    }
}

/// The colour-key blitter leaves keyed pixels alone and writes the rest.
#[test]
fn blit_colour_key_skips_keyed_pixels() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    setup_expanding_blit(&mut engine);
    // Rewrite the map with SDL_COPY_COLORKEY and key 2.
    write_blit_map(
        &mut engine,
        BLIT_MAP,
        DST_SURFACE,
        0,
        SRC_FORMAT,
        DST_FORMAT,
        4,
        16,
        LUT,
        0x100,
        2,
        (1, 1),
    );

    write_guest_rect(&mut engine, RECT, 0, 0, 4, 3);
    let dst_rect = 0x5020_u64;
    write_guest_rect(&mut engine, dst_rect, 0, 0, 4, 3);
    write_regs(&mut engine, SRC_SURFACE, RECT, DST_SURFACE, dst_rect, 0);
    assert_return_value!(
        crate::sdl2::blit::handle_lower_blit(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );

    let out = read_bytes(&mut engine, DST_PIXELS, 48);
    // Column 2 of every row is index 2 (6, 10 for lower rows) — only the exact
    // key value is skipped, so row 0 column 2 stays the fill byte.
    assert_eq!(&out[8..12], &[0xEE; 4], "index 2 in row 0 must be skipped");
    assert_eq!(&out[12..16], &0x0100_0003_u32.to_le_bytes());
    // Row 1 column 2 is index 6, not the key: written.
    assert_eq!(&out[16 + 8..16 + 12], &0x0100_0006_u32.to_le_bytes());
}

/// Identity blits are row copies that respect the surface pitches.
#[test]
fn blit_identity_copies_rows_and_keeps_pitch_gaps() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // Both sides 32bpp with the *same* format object → `map->identity == 1`.
    write_format(&mut engine, SRC_FORMAT, PIXELFORMAT_ARGB8888, 0, 32, 4);
    write_blit_map(
        &mut engine,
        BLIT_MAP,
        DST_SURFACE,
        1,
        SRC_FORMAT,
        SRC_FORMAT,
        16,
        20,
        LUT,
        0,
        0,
        (0, 0),
    );
    write_surface(
        &mut engine,
        SRC_SURFACE,
        0,
        SRC_FORMAT,
        4,
        2,
        16,
        SRC_PIXELS,
        0,
        (0, 0, 4, 2),
        BLIT_MAP,
    );
    // Destination pitch 20 > 16: the gap bytes must stay untouched.
    write_surface(
        &mut engine,
        DST_SURFACE,
        0,
        SRC_FORMAT,
        4,
        2,
        20,
        DST_PIXELS,
        0,
        (0, 0, 4, 2),
        0,
    );
    let src: Vec<u8> = (0_u8..32).collect();
    engine
        .mem_write(SRC_PIXELS, &src)
        .expect("write src pixels");
    engine
        .mem_write(DST_PIXELS, &[0xEE_u8; 40])
        .expect("write dst pixels");
    // Copy only the second row, at destination row 0.
    write_guest_rect(&mut engine, RECT, 0, 1, 4, 1);
    let dst_rect = 0x5030_u64;
    write_guest_rect(&mut engine, dst_rect, 0, 0, 4, 1);
    write_regs(&mut engine, SRC_SURFACE, RECT, DST_SURFACE, dst_rect, 0);
    assert_return_value!(
        crate::sdl2::blit::handle_lower_blit(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );

    let out = read_bytes(&mut engine, DST_PIXELS, 40);
    assert_eq!(&out[0..16], &src[16..32], "row 0 must be source row 1");
    assert_eq!(&out[16..20], &[0xEE; 4], "the pitch gap must be untouched");
    assert_eq!(&out[20..40], &[0xEE; 20], "row 1 must be untouched");
}

/// `SDL_FillRect` fills exactly the clipped rect, with SDL's 32bpp pattern.
#[test]
fn fill_rect_writes_only_the_clipped_rect() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_format(&mut engine, DST_FORMAT, PIXELFORMAT_ARGB8888, 0, 32, 4);
    // 4x3, pitch 16, clip rect inset to {1,1,2,2}.
    write_surface(
        &mut engine,
        DST_SURFACE,
        0,
        DST_FORMAT,
        4,
        3,
        16,
        DST_PIXELS,
        0,
        (1, 1, 2, 2),
        0,
    );
    engine
        .mem_write(DST_PIXELS, &[0xEE_u8; 48])
        .expect("write dst pixels");
    // A rect that extends past the clip rect: clipping must trim it.
    write_guest_rect(&mut engine, RECT, 0, 0, 4, 3);
    write_regs(&mut engine, DST_SURFACE, RECT, 0x1122_3344, 0, 0);
    assert_return_value!(
        crate::sdl2::fill::handle_fill_rect(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );

    let out = read_bytes(&mut engine, DST_PIXELS, 48);
    let filled = 0x1122_3344_u32.to_le_bytes();
    for row in 0..3_usize {
        for column in 0..4_usize {
            let at = row * 16 + column * 4;
            let inside = (1..3).contains(&column) && (1..3).contains(&row);
            if inside {
                assert_eq!(&out[at..at + 4], &filled, "row {row} col {column}");
            } else {
                assert_eq!(
                    &out[at..at + 4],
                    &[0xEE; 4],
                    "outside the clip rect: row {row} col {column}"
                );
            }
        }
    }
}

/// `SDL_FillRect(rect == NULL)` fills the surface's clip rect, not the surface
/// bounds — SDL uses `dst->clip_rect` verbatim.
#[test]
fn fill_rect_with_null_rect_uses_the_clip_rect() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_format(&mut engine, DST_FORMAT, PIXELFORMAT_ARGB8888, 0, 32, 4);
    write_surface(
        &mut engine,
        DST_SURFACE,
        0,
        DST_FORMAT,
        2,
        2,
        8,
        DST_PIXELS,
        0,
        (0, 1, 2, 1),
        0,
    );
    engine
        .mem_write(DST_PIXELS, &[0xEE_u8; 16])
        .expect("write dst pixels");
    write_regs(&mut engine, DST_SURFACE, 0, 0xAABB_CCDD, 0, 0);
    assert_return_value!(
        crate::sdl2::fill::handle_fill_rect(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );

    let out = read_bytes(&mut engine, DST_PIXELS, 16);
    assert_eq!(&out[0..8], &[0xEE; 8], "row 0 is outside the clip rect");
    assert_eq!(&out[8..16], &[0xDD, 0xCC, 0xBB, 0xAA].repeat(2));
}

/// Lock/unlock is the recursive counter and nothing else on a plain surface.
#[test]
fn lock_and_unlock_track_the_recursive_counter() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_format(&mut engine, DST_FORMAT, PIXELFORMAT_ARGB8888, 0, 32, 4);
    write_surface(
        &mut engine,
        DST_SURFACE,
        0,
        DST_FORMAT,
        2,
        2,
        8,
        DST_PIXELS,
        0,
        (0, 0, 2, 2),
        0,
    );
    let locked_of = |engine: &mut IcedCpu| {
        let bytes = read_bytes(engine, DST_SURFACE + 48, 4);
        i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    };

    for expected in 1..=3 {
        write_regs(&mut engine, DST_SURFACE, 0, 0, 0, 0);
        assert_return_value!(
            crate::sdl2::lock_surface(&mut HandlerContext::new(
                &mut engine,
                test_environment(),
                &mut state
            )),
            0
        );
        assert_eq!(locked_of(&mut engine), expected, "lock count");
    }
    for expected in [2, 1, 0] {
        write_regs(&mut engine, DST_SURFACE, 0, 0, 0, 0);
        crate::sdl2::unlock_surface(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state,
        ))
        .expect("unlock must succeed");
        assert_eq!(locked_of(&mut engine), expected, "unlock count");
    }
    // An unlock on an unlocked surface is a no-op, not an underflow.
    write_regs(&mut engine, DST_SURFACE, 0, 0, 0, 0);
    crate::sdl2::unlock_surface(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("unlock must succeed");
    assert_eq!(locked_of(&mut engine), 0);
}

/// A blend-mode blit is not reproduced: the handler must tail-jump to the
/// module's own implementation with the guest's stack and arguments intact.
#[test]
fn unsupported_blit_falls_through_to_the_guest_implementation() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    setup_expanding_blit(&mut engine);
    // `SDL_COPY_BLEND` — per-pixel guest math, so this must decline.
    write_blit_map(
        &mut engine,
        BLIT_MAP,
        DST_SURFACE,
        0,
        SRC_FORMAT,
        DST_FORMAT,
        4,
        16,
        LUT,
        0x10,
        0,
        (1, 1),
    );
    // A recorded site, as the loader would have recorded it. The export stub
    // here is a `jmp qword ptr [rip+0]` pointing at itself, so the tail target
    // is whatever the slot holds.
    let export_va = 0x6000_u64;
    let stub = [0x48_u8, 0xff, 0x25, 0x00, 0x00, 0x00, 0x00];
    engine
        .mem_write(export_va, &stub)
        .expect("write export stub");
    let guest_body = 0x0000_0000_1400_1000_u64;
    engine
        .mem_write(export_va + 7, &guest_body.to_le_bytes())
        .expect("write slot");
    crate::host_exports::record_for_test(
        &mut engine,
        &mut state,
        "SDL2.dll",
        "SDL_LowerBlit",
        0x0000_0000_6a00_1234,
        export_va,
        0x0000_0000_1400_0000,
        0x2000,
    );

    write_guest_rect(&mut engine, RECT, 0, 0, 4, 3);
    let dst_rect = 0x5040_u64;
    write_guest_rect(&mut engine, dst_rect, 0, 0, 4, 3);
    write_regs(&mut engine, SRC_SURFACE, RECT, DST_SURFACE, dst_rect, 0);
    let result = crate::sdl2::blit::handle_lower_blit(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("fall-through must succeed");

    assert_eq!(
        result.return_address, guest_body,
        "the tail target must be the module's own body, not the host handler"
    );
    assert_eq!(
        engine.read_rip().expect("rip"),
        guest_body,
        "RIP must be redirected, not popped"
    );
    assert_eq!(
        engine.read_rsp().expect("rsp"),
        super::STACK_TOP,
        "a tail jump must not consume the guest's return address"
    );
    // Nothing was written: the guest's own code will do it.
    assert_eq!(read_bytes(&mut engine, DST_PIXELS, 48), vec![0xEE_u8; 48]);
}

// ── The intra-module (dispatch-slot) path ──────────────────────────────────
//
// An import-table patch only reaches *other* modules' calls. SDL2 calling its
// own `SDL_LowerBlit` from `SDL_render_sw.c` never touches the IAT: in this
// build every export is a trampoline that reads a writable pointer out of
// `.data` and jumps through it, which is what makes the intra-module callers
// reachable at all. These two tests are the mechanism-level proof:
//
// 1. `record` plants the fake VA in that slot, so the module's own `jmp` lands
//    where the emulator traps;
// 2. if the module rewrites the slot afterwards (SDL2's one-time dynapi init
//    does exactly that), the next call re-arms it *and* falls through to the
//    new body rather than the stale one.
//
// The runtime half — resolving the fake VA back to `(library, name)` and
// stopping there — is shared with every other WinAPI id and is not SDL-specific.

/// Fake guest image holding one export trampoline.
const MODULE_BASE: u64 = 0x8000;
const MODULE_SIZE: usize = 0x2000;
/// `jmp qword ptr [rip+disp32]` at the module's export entry.
const EXPORT_STUB: u64 = 0x8000;
/// The writable pointer the trampoline reads.
const SLOT: u64 = 0x8020;
/// The module's own implementation, as the slot holds it before we plant.
const MODULE_BODY: u64 = 0x8100;

/// Lay out `48 FF 25 <disp32>` at [`EXPORT_STUB`] pointing at [`SLOT`], with
/// `body` in the slot — the exact shape `arm` derives the slot from.
fn write_export_trampoline(engine: &mut IcedCpu, body: u64) {
    let disp = i32::try_from(SLOT - EXPORT_STUB - 7).expect("trampoline disp");
    let mut stub = vec![0x48_u8, 0xff, 0x25];
    stub.extend_from_slice(&disp.to_le_bytes());
    engine.mem_write(EXPORT_STUB, &stub).expect("write stub");
    engine
        .mem_write(SLOT, &body.to_le_bytes())
        .expect("write slot");
}

fn slot_value(engine: &mut IcedCpu) -> u64 {
    let bytes = read_bytes(engine, SLOT, 8);
    u64::from_le_bytes(bytes[..8].try_into().expect("slot bytes"))
}

/// `calls`/`declined` for one served export.
fn counters(export: &str) -> (u64, u64) {
    crate::sdl2::stats()
        .into_iter()
        .find(|(name, _, _)| *name == export)
        .map(|(_, calls, declined)| (calls, declined))
        .expect("served export")
}

/// A module's own call reaches the host handler through the dispatch slot.
#[test]
fn a_module_call_through_the_dispatch_slot_reaches_the_host_handler() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    setup_expanding_blit(&mut engine);
    write_export_trampoline(&mut engine, MODULE_BODY);
    // The planted value has to be a *decodable* fake VA or the emulator would
    // run it as guest code instead of trapping.
    let fake_va = crate::fake_va::encode_unresolved(7);
    crate::host_exports::record_for_test(
        &mut engine,
        &mut state,
        "SDL2.dll",
        "SDL_LowerBlit",
        fake_va,
        EXPORT_STUB,
        MODULE_BASE,
        MODULE_SIZE,
    );
    assert_eq!(
        slot_value(&mut engine),
        fake_va,
        "the module's dispatch slot must now hold the host fake VA, so an \
         intra-module `jmp [slot]` lands on the host handler"
    );
    assert!(
        matches!(
            crate::fake_va::decode(fake_va),
            Some(crate::fake_va::FakeVa::Unresolved(7))
        ),
        "the planted value must decode to a fake dispatch entry"
    );

    let before = counters("SDL_LowerBlit");
    write_guest_rect(&mut engine, RECT, 0, 0, 4, 3);
    let dst_rect = 0x5050_u64;
    write_guest_rect(&mut engine, dst_rect, 0, 0, 4, 3);
    write_regs(&mut engine, SRC_SURFACE, RECT, DST_SURFACE, dst_rect, 0);
    // Exactly what the session calls once the stop decodes to
    // (library, name): the same string dispatcher every WinAPI id uses.
    let outcome = crate::dispatch_winapi(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        "SDL2.dll",
        "SDL_LowerBlit",
    )
    .expect("dispatch must succeed");
    assert_eq!(outcome.return_value, 0);

    let after = counters("SDL_LowerBlit");
    assert_eq!(after.0, before.0 + 1, "the handler must have been entered");
    assert_eq!(
        after.1, before.1,
        "this blit is a palette expansion, so it must not have been declined"
    );
    // And it really did the work: palette-expanded pixels in the destination.
    let out = read_bytes(&mut engine, DST_PIXELS, 48);
    assert_eq!(&out[0..4], &0x0100_0000_u32.to_le_bytes());
    assert_eq!(&out[44..48], &0x0100_000B_u32.to_le_bytes());
}

/// A slot the module rewrites after we planted is re-armed, and the call falls
/// through to the *new* body — not to the stale pointer and not to itself.
#[test]
fn a_rewritten_dispatch_slot_is_re_armed_and_falls_through_to_the_new_body() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    setup_expanding_blit(&mut engine);
    // Blend mode: a case the handler refuses, so it must hand the call back.
    write_blit_map(
        &mut engine,
        BLIT_MAP,
        DST_SURFACE,
        0,
        SRC_FORMAT,
        DST_FORMAT,
        4,
        16,
        LUT,
        0x10,
        0,
        (1, 1),
    );
    write_export_trampoline(&mut engine, MODULE_BODY);
    let fake_va = crate::fake_va::encode_unresolved(7);
    crate::host_exports::record_for_test(
        &mut engine,
        &mut state,
        "SDL2.dll",
        "SDL_LowerBlit",
        fake_va,
        EXPORT_STUB,
        MODULE_BASE,
        MODULE_SIZE,
    );

    // SDL2's one-time dynapi init rewrites every slot with the real body after
    // we may have planted ours.
    let rewritten_body = 0x8200_u64;
    engine
        .mem_write(SLOT, &rewritten_body.to_le_bytes())
        .expect("rewrite slot");

    let before = counters("SDL_LowerBlit");
    write_guest_rect(&mut engine, RECT, 0, 0, 4, 3);
    let dst_rect = 0x5060_u64;
    write_guest_rect(&mut engine, dst_rect, 0, 0, 4, 3);
    write_regs(&mut engine, SRC_SURFACE, RECT, DST_SURFACE, dst_rect, 0);
    let outcome = crate::dispatch_winapi(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        "SDL2.dll",
        "SDL_LowerBlit",
    )
    .expect("dispatch must succeed");

    let after = counters("SDL_LowerBlit");
    assert_eq!(after.0, before.0 + 1, "the handler must have been entered");
    assert_eq!(after.1, before.1 + 1, "the blend blit must be declined");
    assert_eq!(
        outcome.return_address, rewritten_body,
        "the fall-through must target the body the module wrote, not the stale one"
    );
    assert_eq!(
        slot_value(&mut engine),
        fake_va,
        "the slot must be re-armed so the next intra-module call intercepts too"
    );
    assert_eq!(
        read_bytes(&mut engine, DST_PIXELS, 48),
        vec![0xEE_u8; 48],
        "a declined blit must not have written a pixel"
    );
}
