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
/// `SDL_PIXELFORMAT_RGBA4444`.
const PIXELFORMAT_RGBA4444: u32 = 0x1144_4010;
/// `SDL_PIXELFORMAT_RGB565`.
const PIXELFORMAT_RGB565: u32 = 0x1515_1022;
/// `SDL_PIXELFORMAT_RGB24`.
const PIXELFORMAT_RGB24: u32 = 0x1710_1806;
/// `SDL_PIXELFORMAT_BGR24`.
const PIXELFORMAT_BGR24: u32 = 0x1710_1803;
/// A second palette object, for the palette-to-palette (8bpp destination) case.
/// It sits between the 1 KiB table at [`LUT`] and the colour array.
const DST_FORMAT_PALETTE: u64 = 0x2B00;

/// The channel masks `SDL_AllocFormat` fills in for a given pixel format.
///
/// The handler checks that a format's masks and its losses/shifts agree with
/// each other before it builds a palette table from them, so a fixture that
/// leaves the masks zeroed would be rejected as inconsistent rather than
/// exercised. These are the real values; `RGBA4444` and `RGB565` are the
/// asymmetric-mask cases (different widths, and a two- or three-bit loss).
fn masks_for(format: u32) -> [u32; 4] {
    match format {
        // Indexed: no channels at all, so SDL leaves `loss = 8, shift = 0`.
        PIXELFORMAT_INDEX8 => [0; 4],
        PIXELFORMAT_ARGB8888 => [0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0xff00_0000],
        PIXELFORMAT_RGBA4444 => [0xf000, 0x0f00, 0x00f0, 0x000f],
        PIXELFORMAT_RGB565 => [0xf800, 0x07e0, 0x001f, 0],
        // 24-bit: three one-byte channels, in the two orders SDL names.
        PIXELFORMAT_RGB24 => [0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0],
        PIXELFORMAT_BGR24 => [0x0000_00ff, 0x0000_ff00, 0x00ff_0000, 0],
        other => panic!("no channel-mask fixture for pixel format {other:#010x}"),
    }
}

/// Write a guest `SDL_PixelFormat`.
fn write_format(engine: &mut IcedCpu, va: u64, format: u32, palette: u64, bpp: u8, bytes_pp: u8) {
    let masks = masks_for(format);
    let fmt = crate::sdl2::surface::Format::from_masks(format, palette, bpp, bytes_pp, masks);
    let mut bytes = [0_u8; crate::sdl2::surface::FORMAT_SIZE];
    bytes[0..4].copy_from_slice(&fmt.format.to_le_bytes());
    bytes[8..16].copy_from_slice(&fmt.palette.to_le_bytes());
    bytes[16] = fmt.bits_per_pixel;
    bytes[17] = fmt.bytes_per_pixel;
    bytes[20..24].copy_from_slice(&fmt.rmask.to_le_bytes());
    bytes[24..28].copy_from_slice(&fmt.gmask.to_le_bytes());
    bytes[28..32].copy_from_slice(&fmt.bmask.to_le_bytes());
    bytes[32..36].copy_from_slice(&fmt.amask.to_le_bytes());
    bytes[36] = fmt.rloss;
    bytes[37] = fmt.gloss;
    bytes[38] = fmt.bloss;
    bytes[39] = fmt.aloss;
    bytes[40] = fmt.rshift;
    bytes[41] = fmt.gshift;
    bytes[42] = fmt.bshift;
    bytes[43] = fmt.ashift;
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

// ── The in-place palette rebuild ──────────────────────────────────────────
//
// A palette-era guest recolours its 8bpp palette every frame, so SDL's
// `SDL_LowerBlit` sees a stale `src_palette_version` on *every* call and runs
// `SDL_MapSurface` → `Map1toN` before blitting. The host rebuilds those 256
// entries in place instead, which is only safe if the bytes are identical to
// SDL's, so the tests below compare against an independent transcription of the
// SDL code rather than against the implementation they are checking.

/// Where [`write_palette`] puts the colour array (clear of the 1 KiB table at
/// [`LUT`]).
const PALETTE_COLORS: u64 = 0x2C00;

/// A 256-colour palette that exercises every channel independently: a ramp, an
/// inverted ramp, a wrapping product, and an alpha that alternates between
/// opaque and transparent.
fn palette_colors() -> Vec<[u8; 4]> {
    (0..=255_u8)
        .map(|i| {
            [
                i,
                255 - i,
                i.wrapping_mul(3),
                if i % 2 == 0 { 255 } else { 0 },
            ]
        })
        .collect()
}

/// Write a guest `SDL_Palette` with its colours.
fn write_palette(engine: &mut IcedCpu, va: u64, version: u32, colors: &[[u8; 4]]) {
    let mut header = [0_u8; crate::sdl2::surface::PALETTE_SIZE];
    header[0..4].copy_from_slice(
        &i32::try_from(colors.len())
            .expect("palette fits an int")
            .to_le_bytes(),
    );
    header[8..16].copy_from_slice(&PALETTE_COLORS.to_le_bytes());
    header[16..20].copy_from_slice(&version.to_le_bytes());
    engine.mem_write(va, &header).expect("write palette");
    let flat: Vec<u8> = colors.iter().flat_map(|color| *color).collect();
    engine
        .mem_write(PALETTE_COLORS, &flat)
        .expect("write palette colours");
}

/// Write the modulation the mapping was built with (`map->info.{r,g,b,a}`).
fn write_map_mods(engine: &mut IcedCpu, va: u64, mods: [u8; 4]) {
    // `info` starts 32 bytes into the map; `r` is 80 bytes into `info`.
    engine.mem_write(va + 32 + 80, &mods).expect("write mods");
}

/// An independent transcription of `Map1toN` (`SDL_pixels.c`) and
/// `ASSEMBLE_RGBA` (`SDL_blit.h`).
///
/// Written from SDL's sources, not from `crate::sdl2::palette`: comparing an
/// implementation against itself proves nothing, and the failure this guards
/// against (a mask/shift read from the wrong field) is silent — the frame just
/// comes out the wrong colour.
fn reference_map_1_to_n(
    dst: &crate::sdl2::surface::Format,
    palette: &[[u8; 4]],
    mods: [u8; 4],
) -> Vec<u8> {
    // `bpp = (dst->BytesPerPixel == 3) ? 4 : dst->BytesPerPixel`, so a 24-bit
    // destination still gets a four-byte lane, of which three are written.
    let lane = if dst.bytes_per_pixel == 3 {
        4
    } else {
        usize::from(dst.bytes_per_pixel)
    };
    let mut table = vec![0_u8; 256 * lane];
    let scaled = |channel: u8, modulation: u8| -> u8 {
        u8::try_from((u32::from(channel) * u32::from(modulation)) / 255).unwrap_or(0)
    };
    let shifts = [dst.rshift, dst.gshift, dst.bshift, dst.ashift];
    let losses = [dst.rloss, dst.gloss, dst.bloss, dst.aloss];
    for (index, color) in palette.iter().enumerate() {
        let channels = [
            scaled(color[0], mods[0]),
            scaled(color[1], mods[1]),
            scaled(color[2], mods[2]),
            scaled(color[3], mods[3]),
        ];
        let mut entry = [0_u8; 4];
        if dst.bytes_per_pixel == 3 {
            // The 24-bit case bypasses the packing entirely: raw bytes at
            // `shift / 8`, and alpha is never written.
            for channel in 0..3 {
                entry[usize::from(shifts[channel] / 8)] = channels[channel];
            }
        } else {
            let mut pixel = 0_u32;
            for channel in 0..4 {
                pixel |=
                    (u32::from(channels[channel]) >> losses[channel]) << u32::from(shifts[channel]);
            }
            for (byte, slot) in entry.iter_mut().enumerate() {
                *slot = u8::try_from((pixel >> (8 * byte)) & 0xFF).unwrap_or(0);
            }
        }
        table[index * lane..(index + 1) * lane].copy_from_slice(&entry[..lane]);
    }
    table
}

/// Every format these tests build a table for, as `(format, bits, bytes)`.
const TABLE_FORMATS: [(u32, u8, u8); 5] = [
    (PIXELFORMAT_ARGB8888, 32, 4),
    (PIXELFORMAT_RGBA4444, 16, 2),
    (PIXELFORMAT_RGB565, 16, 2),
    (PIXELFORMAT_RGB24, 24, 3),
    (PIXELFORMAT_BGR24, 24, 3),
];

fn format_of(format: u32, bits: u8, bytes: u8) -> crate::sdl2::surface::Format {
    crate::sdl2::surface::Format::from_masks(format, 0, bits, bytes, masks_for(format))
}

/// The rebuild must reproduce SDL's table word for word: all 256 entries, for
/// every destination width, every modulation, and both 24-bit byte orders.
#[test]
fn the_rebuilt_table_matches_map_1_to_n_entry_for_entry() {
    const MODULATIONS: [[u8; 4]; 4] = [
        [255, 255, 255, 255],
        [0, 0, 0, 0],
        [255, 128, 64, 32],
        [200, 150, 100, 255],
    ];
    let colors = palette_colors();
    for (format, bits, bytes) in TABLE_FORMATS {
        let dst = format_of(format, bits, bytes);
        for mods in MODULATIONS {
            let mut engine = test_engine();
            write_format(&mut engine, SRC_FORMAT, PIXELFORMAT_INDEX8, PALETTE, 8, 1);
            write_format(&mut engine, DST_FORMAT, format, 0, bits, bytes);
            write_palette(&mut engine, PALETTE, 9, &colors);
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
                0,
                0,
                (1, 1),
            );
            write_map_mods(&mut engine, BLIT_MAP, mods);
            // Poison the table so a rebuild that skipped entries is visible.
            engine.mem_write(LUT, &[0xCD_u8; 1024]).expect("poison");

            let rebuilt = crate::sdl2::palette::rebuild_table_from_guest(
                &mut engine,
                BLIT_MAP,
                SRC_FORMAT,
                DST_FORMAT,
            )
            .expect("the rebuild must not fail");
            assert!(rebuilt, "format {format:#010x} mods {mods:?} must rebuild");

            let lane = if bytes == 3 { 4 } else { usize::from(bytes) };
            let expected = reference_map_1_to_n(&dst, &colors, mods);
            let got = read_bytes(&mut engine, LUT, 256 * lane);
            assert_eq!(
                got, expected,
                "format {format:#010x} mods {mods:?}: the rebuilt table must be \
                 SDL's, entry for entry"
            );
        }
    }
}

/// `Map1toN` writes only `palette->ncolors` entries; the rest of the `calloc`ed
/// table stays zero. A rebuild that ran past `ncolors` would corrupt them.
#[test]
fn a_rebuild_leaves_the_entries_past_ncolors_alone() {
    let mut engine = test_engine();
    write_format(&mut engine, SRC_FORMAT, PIXELFORMAT_INDEX8, PALETTE, 8, 1);
    write_format(&mut engine, DST_FORMAT, PIXELFORMAT_ARGB8888, 0, 32, 4);
    let colors: Vec<[u8; 4]> = palette_colors().into_iter().take(200).collect();
    write_palette(&mut engine, PALETTE, 4, &colors);
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
        0,
        0,
        (1, 1),
    );
    write_map_mods(&mut engine, BLIT_MAP, [255, 255, 255, 255]);
    engine.mem_write(LUT, &[0xCD_u8; 1024]).expect("poison");

    assert!(
        crate::sdl2::palette::rebuild_table_from_guest(
            &mut engine,
            BLIT_MAP,
            SRC_FORMAT,
            DST_FORMAT
        )
        .expect("rebuild must not fail"),
        "a partial palette is still rebuildable"
    );

    let expected = reference_map_1_to_n(
        &format_of(PIXELFORMAT_ARGB8888, 32, 4),
        &colors,
        [255, 255, 255, 255],
    );
    let got = read_bytes(&mut engine, LUT, 1024);
    assert_eq!(
        &got[..200 * 4],
        &expected[..200 * 4],
        "the first 200 entries"
    );
    assert_eq!(
        &got[200 * 4..],
        &[0xCD_u8; 56 * 4],
        "entries at and past ncolors must not be touched"
    );
}

/// The end-to-end shape DOOM Retro takes: a stale source palette version, a
/// table that already exists, and a blit that must come out natively — with the
/// guest's table rewritten and the version stamped so the next frame is free.
#[test]
fn a_stale_source_palette_is_rebuilt_in_place_and_the_blit_runs_natively() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    setup_expanding_blit(&mut engine);
    let colors = palette_colors();
    // The guest recoloured its palette: version 2, while the mapping was built
    // for version 1 (`setup_expanding_blit`), so `SDL_LowerBlit` sees it stale.
    write_palette(&mut engine, PALETTE, 2, &colors);
    write_map_mods(&mut engine, BLIT_MAP, [255, 255, 255, 255]);

    write_guest_rect(&mut engine, RECT, 0, 0, 4, 3);
    let dst_rect = 0x5070_u64;
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

    let colors_for_reference = colors.clone();
    let expected = reference_map_1_to_n(
        &format_of(PIXELFORMAT_ARGB8888, 32, 4),
        &colors_for_reference,
        [255; 4],
    );
    // The guest's own table now holds SDL's entries ...
    assert_eq!(
        read_bytes(&mut engine, LUT, 1024),
        expected,
        "the rebuild must happen in the guest's table, not in a copy"
    );
    // ... the blit read that table ...
    let out = read_bytes(&mut engine, DST_PIXELS, 48);
    for row in 0..3_usize {
        for column in 0..4_usize {
            let index = row * 4 + column;
            let at = row * 16 + column * 4;
            assert_eq!(
                &out[at..at + 4],
                &expected[index * 4..index * 4 + 4],
                "row {row} column {column}: source index {index}"
            );
        }
    }
    // ... and the mapping was stamped, so SDL thinks it is current.
    let stamped = read_bytes(&mut engine, BLIT_MAP + 124, 4);
    assert_eq!(
        u32::from_le_bytes(stamped[..4].try_into().expect("word")),
        2,
        "map->src_palette_version must be stamped with the palette's version"
    );

    // Next frame: the version matches, so nothing is rebuilt and the table is
    // used as-is. Poisoning it is how that is made visible: the blit must read
    // the poison rather than regenerate the table.
    engine.mem_write(LUT, &[0x5A_u8; 1024]).expect("poison");
    write_regs(&mut engine, SRC_SURFACE, RECT, DST_SURFACE, dst_rect, 0);
    assert_return_value!(
        crate::sdl2::blit::handle_lower_blit(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    assert_eq!(
        read_bytes(&mut engine, LUT, 1024),
        vec![0x5A_u8; 1024],
        "a fresh mapping must not be rebuilt again"
    );
    assert_eq!(
        read_bytes(&mut engine, DST_PIXELS, 48),
        vec![0x5A_u8; 48],
        "the second blit must use the table as it found it"
    );
}

/// Frame one: `map->info.table` is NULL, so SDL has to allocate it. There is
/// nothing to rebuild in place, and the handler must stay out of the way.
#[test]
fn a_stale_palette_with_no_table_stays_with_the_guest() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    setup_expanding_blit(&mut engine);
    let colors = palette_colors();
    write_palette(&mut engine, PALETTE, 2, &colors);
    // No table yet — the mapping the guest is about to build.
    write_blit_map(
        &mut engine,
        BLIT_MAP,
        DST_SURFACE,
        0,
        SRC_FORMAT,
        DST_FORMAT,
        4,
        16,
        0,
        0,
        0,
        (1, 1),
    );
    let guest_body = record_export_site(&mut engine, &mut state, "SDL_LowerBlit");

    write_guest_rect(&mut engine, RECT, 0, 0, 4, 3);
    let dst_rect = 0x5080_u64;
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
        "a NULL table means SDL must allocate it: the call goes back to the guest"
    );
    assert_eq!(
        read_bytes(&mut engine, DST_PIXELS, 48),
        vec![0xEE_u8; 48],
        "a declined blit must not have written a pixel"
    );
    let stamped = read_bytes(&mut engine, BLIT_MAP + 124, 4);
    assert_eq!(
        u32::from_le_bytes(stamped[..4].try_into().expect("word")),
        1,
        "a declined call must not stamp the mapping"
    );
}

/// A stale *destination* palette needs `Map1to1` — a nearest-colour search over
/// the destination palette — which is guest work.
#[test]
fn a_stale_destination_palette_stays_with_the_guest() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_format(&mut engine, SRC_FORMAT, PIXELFORMAT_INDEX8, PALETTE, 8, 1);
    // Both sides 8bpp: a palette-to-palette blit.
    write_format(
        &mut engine,
        DST_FORMAT,
        PIXELFORMAT_INDEX8,
        DST_FORMAT_PALETTE,
        8,
        1,
    );
    write_palette(&mut engine, PALETTE, 5, &palette_colors());
    write_palette(&mut engine, DST_FORMAT_PALETTE, 7, &palette_colors());
    // The mapping was built when the destination palette was at version 3.
    write_blit_map(
        &mut engine,
        BLIT_MAP,
        DST_SURFACE,
        0,
        SRC_FORMAT,
        DST_FORMAT,
        4,
        4,
        LUT,
        0,
        0,
        (3, 5),
    );
    write_surface(
        &mut engine,
        SRC_SURFACE,
        0,
        SRC_FORMAT,
        4,
        1,
        4,
        SRC_PIXELS,
        0,
        (0, 0, 4, 1),
        BLIT_MAP,
    );
    write_surface(
        &mut engine,
        DST_SURFACE,
        0,
        DST_FORMAT,
        4,
        1,
        4,
        DST_PIXELS,
        0,
        (0, 0, 4, 1),
        0,
    );
    engine.mem_write(SRC_PIXELS, &[0, 1, 2, 3]).expect("src");
    engine.mem_write(DST_PIXELS, &[0xEE_u8; 4]).expect("dst");
    let guest_body = record_export_site(&mut engine, &mut state, "SDL_LowerBlit");

    write_guest_rect(&mut engine, RECT, 0, 0, 4, 1);
    let dst_rect = 0x5090_u64;
    write_guest_rect(&mut engine, dst_rect, 0, 0, 4, 1);
    write_regs(&mut engine, SRC_SURFACE, RECT, DST_SURFACE, dst_rect, 0);
    let result = crate::sdl2::blit::handle_lower_blit(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("fall-through must succeed");

    assert_eq!(
        result.return_address, guest_body,
        "a stale destination palette needs Map1to1, which is guest work"
    );
    assert_eq!(read_bytes(&mut engine, DST_PIXELS, 4), vec![0xEE_u8; 4]);
}

/// A format whose masks and shifts disagree means one of the two struct reads
/// is off by a field; every colour built from it would be wrong silently.
#[test]
fn a_format_with_inconsistent_channels_is_never_used() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    setup_expanding_blit(&mut engine);
    // A destination whose `Gshift` was read from `Bshift`: the masks no longer
    // reconstruct, which is exactly the failure the gate exists for.
    let masks = masks_for(PIXELFORMAT_ARGB8888);
    let mut torn = [0_u8; crate::sdl2::surface::FORMAT_SIZE];
    torn[0..4].copy_from_slice(&PIXELFORMAT_ARGB8888.to_le_bytes());
    torn[16] = 32;
    torn[17] = 4;
    torn[20..24].copy_from_slice(&masks[0].to_le_bytes());
    torn[24..28].copy_from_slice(&masks[1].to_le_bytes());
    torn[28..32].copy_from_slice(&masks[2].to_le_bytes());
    torn[32..36].copy_from_slice(&masks[3].to_le_bytes());
    torn[36] = 0;
    torn[37] = 0;
    torn[38] = 0;
    torn[39] = 0;
    torn[40] = 16;
    torn[41] = 0; // must be 8: `Gshift` read from the wrong slot.
    torn[42] = 0;
    torn[43] = 24;
    engine.mem_write(DST_FORMAT, &torn).expect("write format");
    let guest_body = record_export_site(&mut engine, &mut state, "SDL_LowerBlit");

    write_guest_rect(&mut engine, RECT, 0, 0, 4, 3);
    let dst_rect = 0x50A0_u64;
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
        "an inconsistent format must never be used to build a table"
    );
    assert_eq!(read_bytes(&mut engine, DST_PIXELS, 48), vec![0xEE_u8; 48]);
}

/// Record an `SDL_LowerBlit` export site so a decline can be observed as the
/// actual tail jump the emulator performs, rather than inferred from counters
/// that other tests are incrementing at the same time.
fn record_export_site(engine: &mut IcedCpu, state: &mut WinApiState, export: &str) -> u64 {
    let export_va = 0x6000_u64;
    let stub = [0x48_u8, 0xff, 0x25, 0x00, 0x00, 0x00, 0x00];
    engine
        .mem_write(export_va, &stub)
        .expect("write export stub");
    // Inside the recorded image (`base + size`), which is what makes `arm`
    // treat the slot as a live implementation pointer and fall through to it.
    let guest_body = 0x0000_0000_1400_1000_u64;
    engine
        .mem_write(export_va + 7, &guest_body.to_le_bytes())
        .expect("write slot");
    crate::host_exports::record_for_test(
        engine,
        state,
        "SDL2.dll",
        export,
        0x0000_0000_6a00_5678,
        export_va,
        0x0000_0000_1400_0000,
        0x2000,
    );
    guest_body
}

// ── SDL_SoftStretch (the scaled present) ──────────────────────────────────
//
// `SDL_BlitScaled` on same-format surfaces is not a blitter at all: it is
// `SDL_SoftStretch`'s own nearest scaler, and its per-pixel 16.16 stepping is
// the largest single consumer in the DOOM Retro profile. The comparison is
// against a transcription of `scale_mat_nearest_N`, never against ourselves.

/// Formats whose bytes-per-pixel covers SDL's four `scale_mat_nearest_N`
/// variants, with the matching width in bytes.
const STRETCH_FORMATS: [(u32, u8, u8); 4] = [
    (PIXELFORMAT_INDEX8, 8, 1),
    (PIXELFORMAT_RGB565, 16, 2),
    (PIXELFORMAT_RGB24, 24, 3),
    (PIXELFORMAT_ARGB8888, 32, 4),
];

/// Build a same-format source and destination pair for one stretch.
#[expect(
    clippy::too_many_arguments,
    reason = "one argument per stretch dimension; a struct would restate them"
)]
fn run_stretch(
    engine: &mut IcedCpu,
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
    bpp: usize,
    format: u32,
    bits: u8,
) {
    write_format(
        engine,
        SRC_FORMAT,
        format,
        0,
        bits,
        u8::try_from(bpp).unwrap(),
    );
    write_format(
        engine,
        DST_FORMAT,
        format,
        0,
        bits,
        u8::try_from(bpp).unwrap(),
    );
    let src_pitch = i32::try_from(src_w * bpp).unwrap();
    let dst_pitch = i32::try_from(dst_w * bpp).unwrap();
    write_surface(
        engine,
        SRC_SURFACE,
        0,
        SRC_FORMAT,
        i32::try_from(src_w).unwrap(),
        i32::try_from(src_h).unwrap(),
        src_pitch,
        SRC_PIXELS,
        0,
        (
            0,
            0,
            u16::try_from(src_w).unwrap(),
            u16::try_from(src_h).unwrap(),
        ),
        BLIT_MAP,
    );
    write_surface(
        engine,
        DST_SURFACE,
        0,
        DST_FORMAT,
        i32::try_from(dst_w).unwrap(),
        i32::try_from(dst_h).unwrap(),
        dst_pitch,
        DST_PIXELS,
        0,
        (
            0,
            0,
            u16::try_from(dst_w).unwrap(),
            u16::try_from(dst_h).unwrap(),
        ),
        0,
    );
    // A source pattern with no repeating period inside a row, so a one-pixel
    // stepping error cannot pass by luck.
    let source: Vec<u8> = (0..src_pitch as usize * src_h)
        .map(|i| u8::try_from((i * 7 + 13) % 251).unwrap_or(0))
        .collect();
    engine.mem_write(SRC_PIXELS, &source).expect("src pixels");
    let dst_bytes = dst_pitch as usize * dst_h;
    engine
        .mem_write(DST_PIXELS, &vec![0xEE_u8; dst_bytes])
        .expect("dst pixels");
}

/// The stepper must be SDL's, for every destination width SDL has a scaler for
/// and for the ratios where a from-zero stepper silently shifts every column.
#[test]
fn a_nearest_stretch_reproduces_sdls_stepping() {
    for (format, bits, bpp) in STRETCH_FORMATS {
        for (src_w, src_h, dst_w, dst_h) in [
            (5, 4, 8, 7),
            (3, 2, 2, 3),
            (7, 3, 3, 7),
            (9, 9, 2, 2),
            (6, 5, 6, 5),
        ] {
            let mut engine = test_engine();
            let mut state = default_winapi_state();
            run_stretch(
                &mut engine,
                src_w,
                src_h,
                dst_w,
                dst_h,
                usize::from(bpp),
                format,
                bits,
            );
            let src_rect = 0x5100_u64;
            let dst_rect = 0x5110_u64;
            write_guest_rect(
                &mut engine,
                src_rect,
                0,
                0,
                u16::try_from(src_w).unwrap(),
                u16::try_from(src_h).unwrap(),
            );
            write_guest_rect(
                &mut engine,
                dst_rect,
                0,
                0,
                u16::try_from(dst_w).unwrap(),
                u16::try_from(dst_h).unwrap(),
            );
            write_regs(&mut engine, SRC_SURFACE, src_rect, DST_SURFACE, dst_rect, 0);
            assert_return_value!(
                crate::sdl2::stretch::handle_soft_stretch(&mut HandlerContext::new(
                    &mut engine,
                    test_environment(),
                    &mut state
                )),
                0
            );

            let bpp = usize::from(bpp);
            let source = read_bytes(&mut engine, SRC_PIXELS, src_w * bpp * src_h);
            let expected = crate::sdl2::stretch::reference::nearest(
                &source,
                src_w,
                src_h,
                src_w * bpp,
                dst_w,
                dst_h,
                dst_w * bpp,
                bpp,
                0xEE,
            );
            let got = read_bytes(&mut engine, DST_PIXELS, dst_w * bpp * dst_h);
            assert_eq!(
                got, expected,
                "format {format:#010x} {src_w}x{src_h} -> {dst_w}x{dst_h}: \
                 the stretch must be SDL's, pixel for pixel"
            );
        }
    }
}

/// A NULL rect means the whole surface, on both sides — SDL substitutes one and
/// the scaler then covers `src->w x src->h` into `dst->w x dst->h`.
#[test]
fn a_stretch_with_null_rects_covers_both_surfaces() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    run_stretch(&mut engine, 4, 4, 6, 6, 4, PIXELFORMAT_ARGB8888, 32);
    write_regs(&mut engine, SRC_SURFACE, 0, DST_SURFACE, 0, 0);
    assert_return_value!(
        crate::sdl2::stretch::handle_soft_stretch(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    let source = read_bytes(&mut engine, SRC_PIXELS, 4 * 4 * 4);
    let expected = crate::sdl2::stretch::reference::nearest(&source, 4, 4, 16, 6, 6, 24, 4, 0xEE);
    assert_eq!(read_bytes(&mut engine, DST_PIXELS, 6 * 6 * 4), expected);
}

/// A destination rect with no area is a documented success that touches
/// nothing — not an error and not a write.
#[test]
fn an_empty_stretch_destination_is_a_no_op_success() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    run_stretch(&mut engine, 4, 4, 6, 6, 4, PIXELFORMAT_ARGB8888, 32);
    write_guest_rect(&mut engine, 0x5120, 0, 0, 6, 0);
    write_regs(&mut engine, SRC_SURFACE, 0, DST_SURFACE, 0x5120, 0);
    assert_return_value!(
        crate::sdl2::stretch::handle_soft_stretch(&mut HandlerContext::new(
            &mut engine,
            test_environment(),
            &mut state
        )),
        0
    );
    assert_eq!(
        read_bytes(&mut engine, DST_PIXELS, 6 * 6 * 4),
        vec![0xEE_u8; 6 * 6 * 4],
        "an empty stretch must not write a pixel"
    );
}

/// `SDL_UpperSoftStretch` refuses two different formats with an error string;
/// the handler must hand the call back rather than guess at a conversion.
#[test]
fn a_stretch_between_different_formats_stays_with_the_guest() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    run_stretch(&mut engine, 4, 4, 6, 6, 4, PIXELFORMAT_ARGB8888, 32);
    // Same width, different format: the pair SDL errors on.
    write_format(&mut engine, DST_FORMAT, PIXELFORMAT_RGBA4444, 0, 16, 2);
    let guest_body = record_export_site(&mut engine, &mut state, "SDL_SoftStretch");
    write_guest_rect(&mut engine, 0x5130, 0, 0, 4, 4);
    write_guest_rect(&mut engine, 0x5140, 0, 0, 6, 6);
    write_regs(&mut engine, SRC_SURFACE, 0x5130, DST_SURFACE, 0x5140, 0);
    let result = crate::sdl2::stretch::handle_soft_stretch(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("fall-through must succeed");
    assert_eq!(
        result.return_address, guest_body,
        "a mismatched pair is SDL's error to report, not ours to convert"
    );
}

/// A rect that leaves its surface is SDL's "Invalid blit rectangle" error; the
/// handler must not scale a single pixel of it.
#[test]
fn a_stretch_rect_outside_its_surface_stays_with_the_guest() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    run_stretch(&mut engine, 4, 4, 6, 6, 4, PIXELFORMAT_ARGB8888, 32);
    let guest_body = record_export_site(&mut engine, &mut state, "SDL_SoftStretch");
    // Source rect starts inside and runs two columns past the right edge.
    write_guest_rect(&mut engine, 0x5150, 2, 0, 4, 4);
    write_guest_rect(&mut engine, 0x5160, 0, 0, 6, 6);
    write_regs(&mut engine, SRC_SURFACE, 0x5150, DST_SURFACE, 0x5160, 0);
    let result = crate::sdl2::stretch::handle_soft_stretch(&mut HandlerContext::new(
        &mut engine,
        test_environment(),
        &mut state,
    ))
    .expect("fall-through must succeed");
    assert_eq!(result.return_address, guest_body);
    assert_eq!(
        read_bytes(&mut engine, DST_PIXELS, 6 * 6 * 4),
        vec![0xEE_u8; 6 * 6 * 4],
        "a refused stretch must not have written a pixel"
    );
}

/// `SDL_SoftStretch` must be wired all the way through the string dispatcher,
/// not just present in `EXPORTS`: the export census test proves there is an arm,
/// this proves the arm is the stretch handler (a NULL surface declines through
/// it, which the "unsupported" path could never do).
#[test]
fn the_soft_stretch_export_dispatches_to_the_stretch_handler() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    run_stretch(&mut engine, 4, 4, 6, 6, 4, PIXELFORMAT_ARGB8888, 32);
    let guest_body = record_export_site(&mut engine, &mut state, "SDL_SoftStretch");
    // NULL source surface: SDL dereferences it and faults, so the handler must
    // hand the call back instead of touching memory.
    write_regs(&mut engine, 0, 0, DST_SURFACE, 0, 0);
    let outcome = crate::dispatch_winapi(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        "SDL2.dll",
        "SDL_SoftStretch",
    )
    .expect("dispatch must succeed");
    assert_eq!(
        outcome.return_address, guest_body,
        "a NULL surface must fall through to SDL, through this export's own handler"
    );
    assert_eq!(
        read_bytes(&mut engine, DST_PIXELS, 6 * 6 * 4),
        vec![0xEE_u8; 6 * 6 * 4]
    );
}

// ── The intra-module detour (the mechanism, tested as a mechanism) ─────────
//
// A guest DLL that calls its own function *by address* (`callq 0x1800d8040`)
// never touches the dynapi slot, so planting a fake VA in the slot cannot reach
// it. The fix is an entry patch plus a relocated prologue, and the claim it
// rests on is general: **any** direct intra-module call to a patched entry is
// interceptable. These tests are that claim, on a fixture module with no import
// table involvement at all.

/// A second fake module image, big enough to hold a trampoline tail.
const INTERNAL_BASE: u64 = 0x8000;
const INTERNAL_SIZE: usize = 0x2000;
/// The module's own internal implementation.
const INTERNAL_BODY: u64 = 0x8200;
/// A caller inside the same module: `call INTERNAL_BODY`.
const INTERNAL_CALLER: u64 = 0x8400;
/// The export trampoline that reaches the same body through a slot.
const INTERNAL_EXPORT: u64 = 0x8100;

/// The 12-byte prologue of `SDL_SoftStretch`'s real body, read off the guest
/// binary (`0x1800d8040`): `sub rsp, 0x38` then `mov dword [rsp+0x20], 0`.
const INTERNAL_PROLOGUE: [u8; 12] = [
    0x48, 0x83, 0xec, 0x38, // sub rsp, 0x38
    0xc7, 0x44, 0x24, 0x20, 0x00, 0x00, 0x00, 0x00, // mov dword [rsp+0x20], 0
];

/// Lay out the fixture module: a body with a real prologue, an export
/// trampoline over a slot that points at it, and a direct `call` from another
/// function in the same module.
fn write_internal_module(engine: &mut IcedCpu, prologue: &[u8; 12]) {
    engine
        .mem_write(INTERNAL_BODY, prologue)
        .expect("write body prologue");
    // `mov eax, 1; ret` after the prologue, so the body is a real function.
    engine
        .mem_write(INTERNAL_BODY + 12, &[0xb8, 0x01, 0x00, 0x00, 0x00, 0xc3])
        .expect("write body");
    // `call INTERNAL_BODY; ret` — the direct, intra-module call.
    let mut caller = vec![0xe8_u8];
    let displacement = i32::try_from(INTERNAL_BODY).expect("body va")
        - i32::try_from(INTERNAL_CALLER + 5).expect("caller va");
    caller.extend_from_slice(&displacement.to_le_bytes());
    caller.push(0xc3);
    engine
        .mem_write(INTERNAL_CALLER, &caller)
        .expect("write caller");
    // The export: `jmp qword ptr [rip+disp32]` over a slot holding the body.
    let slot = INTERNAL_EXPORT + 7 + 0x40;
    let disp =
        i32::try_from(slot).expect("slot") - i32::try_from(INTERNAL_EXPORT + 7).expect("stub");
    let mut stub = vec![0x48_u8, 0xff, 0x25];
    stub.extend_from_slice(&disp.to_le_bytes());
    engine
        .mem_write(INTERNAL_EXPORT, &stub)
        .expect("write stub");
    engine
        .mem_write(slot, &INTERNAL_BODY.to_le_bytes())
        .expect("write slot");
}

fn internal_site(engine: &mut IcedCpu, state: &mut WinApiState) -> u64 {
    let fake_va = crate::fake_va::encode_unresolved(11);
    crate::host_exports::record_for_test(
        engine,
        state,
        "SDL2.dll",
        "SDL_SoftStretch",
        fake_va,
        INTERNAL_EXPORT,
        INTERNAL_BASE,
        INTERNAL_SIZE,
    );
    fake_va
}

/// The whole point: after the detour is planted, the module's *direct* call to
/// its own body lands on the host handler, and a decline resumes in the
/// relocated prologue rather than in the patch.
#[test]
fn a_direct_intra_module_call_is_intercepted_by_the_detour() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_internal_module(&mut engine, &INTERNAL_PROLOGUE);
    let fake_va = internal_site(&mut engine, &mut state);

    // The first fall-through is also the install, so this is the call that has
    // to resume in the trampoline: resuming at the entry would jump straight
    // back into the patch that re-enters this handler (an infinite loop on the
    // real guest, and the reason the install runs *before* the return address
    // is chosen).
    write_regs(&mut engine, 0, 0, 0, 0, 0);
    let first = crate::host_exports::fall_through(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        "SDL2.dll",
        "SDL_SoftStretch",
    )
    .expect("fall-through must succeed");
    let trampoline_va = INTERNAL_BASE + u64::try_from(INTERNAL_SIZE).expect("size") - 32;
    assert_eq!(
        first.return_address, trampoline_va,
        "a decline resumes in the relocated prologue on the very call that plants it"
    );

    // The body's entry is now `mov rax, fake; jmp rax`.
    let patched = read_bytes(&mut engine, INTERNAL_BODY, 12);
    let mut expected = vec![0x48_u8, 0xb8];
    expected.extend_from_slice(&fake_va.to_le_bytes());
    expected.extend_from_slice(&[0xff, 0xe0]);
    assert_eq!(
        patched, expected,
        "the direct call must now land on the host handler's fake VA"
    );

    // The displaced prologue, plus a jump back into the body, lives in the
    // image tail — the same RWX page class as every other guest code byte.
    let trampoline = read_bytes(&mut engine, trampoline_va, 24);
    assert_eq!(
        &trampoline[..12],
        &INTERNAL_PROLOGUE,
        "the relocated prologue must be the bytes we displaced"
    );
    let mut jump_back = vec![0x48_u8, 0xb8];
    jump_back.extend_from_slice(&(INTERNAL_BODY + 12).to_le_bytes());
    jump_back.extend_from_slice(&[0xff, 0xe0]);
    assert_eq!(
        &trampoline[12..24],
        &jump_back[..],
        "and then rejoin the body"
    );

    // A second decline resumes in the trampoline, not in the patch (which would
    // loop) and not in the body (whose prologue is gone).
    write_regs(&mut engine, 0, 0, 0, 0, 0);
    let second = crate::host_exports::fall_through(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        "SDL2.dll",
        "SDL_SoftStretch",
    )
    .expect("fall-through must succeed");
    assert_eq!(
        second.return_address, trampoline_va,
        "a declined detoured call must resume in the relocated prologue"
    );
}

/// The shape DOOM Retro actually has: an export the guest never imports and
/// never looks up, so there is **no dispatch slot at all**, and the module's own
/// code calls the export's entry by direct address.
///
/// This is the case a slot-derived patch rule silently skips: the export is
/// served from the import-table side, `arm` finds no slot to derive, and the
/// detour is never planted — leaving the largest block in the render phase on
/// the emulated path while every counter says the interception is working.
#[test]
fn a_slotless_export_is_detoured_at_its_own_entry() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // The export entry *is* the body: no thunk, no writable pointer.
    write_internal_module(&mut engine, &INTERNAL_PROLOGUE);
    let fake_va = crate::fake_va::encode_unresolved(12);
    crate::host_exports::record_for_test(
        &mut engine,
        &mut state,
        "SDL2.dll",
        "SDL_SoftStretch",
        fake_va,
        INTERNAL_BODY,
        INTERNAL_BASE,
        INTERNAL_SIZE,
    );
    let site = state
        .module_state
        .host_exports
        .values()
        .next()
        .copied()
        .expect("a recorded site");
    assert_eq!(
        site.slot_va, 0,
        "the fixture must model an export with no dispatch slot"
    );

    write_regs(&mut engine, 0, 0, 0, 0, 0);
    let result = crate::host_exports::fall_through(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        "SDL2.dll",
        "SDL_SoftStretch",
    )
    .expect("fall-through must succeed");

    let mut expected = vec![0x48_u8, 0xb8];
    expected.extend_from_slice(&fake_va.to_le_bytes());
    expected.extend_from_slice(&[0xff, 0xe0]);
    assert_eq!(
        read_bytes(&mut engine, INTERNAL_BODY, 12),
        expected,
        "the entry must be patched even though no slot points at it"
    );
    let trampoline_va = INTERNAL_BASE + u64::try_from(INTERNAL_SIZE).expect("size") - 32;
    assert_eq!(
        &read_bytes(&mut engine, trampoline_va, 12),
        &INTERNAL_PROLOGUE,
        "the displaced prologue lives in the image tail"
    );
    assert_eq!(
        result.return_address, trampoline_va,
        "the declined call resumes in the relocated prologue"
    );
}

/// The shape DOOM Retro actually has, and the reason the detour never fired:
/// the export entry is a `jmp qword ptr [rip+disp32]` thunk over a dispatch slot
/// that lives **outside** the module image (SDL2's dynapi arena), and that slot
/// still holds zero when the export is recorded at load time.
///
/// The body is therefore unknown at record time and becomes known only when the
/// module initialises the slot, so the patch target has to be re-derived at
/// *install* time. Remembering the load-time value instead retires the site as
/// unpatchable — which is what the profile reported as `0 planted, 1 refused`.
#[test]
fn a_thunk_export_whose_slot_initialises_late_still_gets_detoured() {
    const OUTSIDE_SLOT: u64 = INTERNAL_BASE + INTERNAL_SIZE as u64 + 0x100;
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_internal_module(&mut engine, &INTERNAL_PROLOGUE);
    // Re-point the export thunk at a slot outside the image and leave it zero:
    // the dynapi table is not initialised yet.
    let disp = i32::try_from(OUTSIDE_SLOT).expect("slot")
        - i32::try_from(INTERNAL_EXPORT + 7).expect("stub");
    let mut stub = vec![0x48_u8, 0xff, 0x25];
    stub.extend_from_slice(&disp.to_le_bytes());
    engine
        .mem_write(INTERNAL_EXPORT, &stub)
        .expect("rewrite export thunk");
    engine
        .mem_write(OUTSIDE_SLOT, &0_u64.to_le_bytes())
        .expect("zero the dispatch slot");

    let fake_va = crate::fake_va::encode_unresolved(13);
    crate::host_exports::record_for_test(
        &mut engine,
        &mut state,
        "SDL2.dll",
        "SDL_SoftStretch",
        fake_va,
        INTERNAL_EXPORT,
        INTERNAL_BASE,
        INTERNAL_SIZE,
    );
    let site = state
        .module_state
        .host_exports
        .values()
        .next()
        .copied()
        .expect("a recorded site");
    assert_eq!(
        site.slot_va, 0,
        "a slot holding zero must not be armed as the live body"
    );

    // The module's own init fills the slot after the export was recorded.
    engine
        .mem_write(OUTSIDE_SLOT, &INTERNAL_BODY.to_le_bytes())
        .expect("initialise the dispatch slot");

    write_regs(&mut engine, 0, 0, 0, 0, 0);
    let result = crate::host_exports::fall_through(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        "SDL2.dll",
        "SDL_SoftStretch",
    )
    .expect("fall-through must succeed");

    let mut expected = vec![0x48_u8, 0xb8];
    expected.extend_from_slice(&fake_va.to_le_bytes());
    expected.extend_from_slice(&[0xff, 0xe0]);
    assert_eq!(
        read_bytes(&mut engine, INTERNAL_BODY, 12),
        expected,
        "the body behind a late-initialised slot must still be patched"
    );
    let trampoline_va = INTERNAL_BASE + u64::try_from(INTERNAL_SIZE).expect("size") - 32;
    assert_eq!(
        result.return_address, trampoline_va,
        "the declined call resumes in the relocated prologue"
    );
}

/// The named-internal-entry path, which is the only one that reaches the block
/// the attribution doc implicates: the audited body is not what the export
/// thunk or its dispatch slot points at, so its address is declared and its
/// first bytes are the guard.
#[test]
fn a_named_internal_entry_is_patched_only_when_its_prologue_matches() {
    // Matching bytes: the declared entry is patched and the call is detoured.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_internal_module(&mut engine, &INTERNAL_PROLOGUE);
    let fake_va = crate::fake_va::encode_unresolved(14);
    crate::host_exports::record_for_test(
        &mut engine,
        &mut state,
        "SDL2.dll",
        "SDL_SoftStretch",
        fake_va,
        INTERNAL_EXPORT,
        INTERNAL_BASE,
        INTERNAL_SIZE,
    );
    crate::host_exports::set_internal_entry(
        &mut state,
        "SDL2.dll",
        "SDL_SoftStretch",
        INTERNAL_BODY,
        INTERNAL_PROLOGUE,
    );
    write_regs(&mut engine, 0, 0, 0, 0, 0);
    let result = crate::host_exports::fall_through(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        "SDL2.dll",
        "SDL_SoftStretch",
    )
    .expect("fall-through must succeed");
    let trampoline_va = INTERNAL_BASE + u64::try_from(INTERNAL_SIZE).expect("size") - 32;
    assert_eq!(
        result.return_address, trampoline_va,
        "a named internal entry must be detoured"
    );

    // A different build: the bytes at the declared entry are not the audited
    // function, so nothing may be patched.
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    write_internal_module(&mut engine, &INTERNAL_PROLOGUE);
    let before = read_bytes(&mut engine, INTERNAL_BODY, 12);
    let fake_va = crate::fake_va::encode_unresolved(15);
    crate::host_exports::record_for_test(
        &mut engine,
        &mut state,
        "SDL2.dll",
        "SDL_SoftStretch",
        fake_va,
        INTERNAL_EXPORT,
        INTERNAL_BASE,
        INTERNAL_SIZE,
    );
    let mut wrong = INTERNAL_PROLOGUE;
    wrong[11] = 0xff;
    crate::host_exports::set_internal_entry(
        &mut state,
        "SDL2.dll",
        "SDL_SoftStretch",
        INTERNAL_BODY,
        wrong,
    );
    write_regs(&mut engine, 0, 0, 0, 0, 0);
    let refused = crate::host_exports::fall_through(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        "SDL2.dll",
        "SDL_SoftStretch",
    )
    .expect("fall-through must succeed");
    assert_eq!(
        read_bytes(&mut engine, INTERNAL_BODY, 12),
        before,
        "a prologue that is not the audited function must be left untouched"
    );
    assert!(refused.return_address != trampoline_va);
}

/// A prologue that cannot be relocated must be left alone. This is the refusal
/// that matters: copying a RIP-relative instruction to another address silently
/// corrupts the guest, and an unpatched function still works.
#[test]
fn a_prologue_that_cannot_be_relocated_is_refused() {
    let mut engine = test_engine();
    let mut state = default_winapi_state();
    // `mov rax, [rip+disp32]` — address-dependent, so it must never be copied.
    let rip_relative: [u8; 12] = [
        0x48, 0x8b, 0x05, 0x10, 0x00, 0x00, 0x00, 0x90, 0x90, 0x90, 0x90, 0x90,
    ];
    write_internal_module(&mut engine, &rip_relative);
    let before = read_bytes(&mut engine, INTERNAL_BODY, 12);
    let _fake = internal_site(&mut engine, &mut state);

    write_regs(&mut engine, 0, 0, 0, 0, 0);
    let result = crate::host_exports::fall_through(
        &mut HandlerContext::new(&mut engine, test_environment(), &mut state),
        "SDL2.dll",
        "SDL_SoftStretch",
    )
    .expect("fall-through must succeed");

    assert_eq!(
        result.return_address, INTERNAL_BODY,
        "a refused detour falls through to the body itself"
    );
    assert_eq!(
        read_bytes(&mut engine, INTERNAL_BODY, 12),
        before,
        "a refused detour must leave the guest's code byte-identical"
    );
    // And no trampoline was written into the image tail.
    let tail = INTERNAL_BASE + u64::try_from(INTERNAL_SIZE).expect("size") - 32;
    assert_eq!(read_bytes(&mut engine, tail, 24), vec![0u8; 24]);
}

/// The relocator accepts the real `SDL_SoftStretch` prologue and rejects the
/// two forms that cannot move: a branch and a RIP-relative operand.
#[test]
fn the_prologue_relocator_whitelists_only_position_independent_bytes() {
    use crate::host_exports::relocatable_prologue_for_test as relocatable;
    assert_eq!(relocatable(&INTERNAL_PROLOGUE), Some(12));
    // push rbx; sub rsp, 0x28; mov [rsp+0x30], rbx  (a classic MSVC prologue)
    assert_eq!(
        relocatable(&[
            0x53, 0x48, 0x83, 0xec, 0x28, 0x48, 0x89, 0x5c, 0x24, 0x30, 0x90, 0x90
        ]),
        Some(12)
    );
    // A relative branch inside the patch window: refused.
    assert_eq!(
        relocatable(&[
            0xe8, 0x00, 0x00, 0x00, 0x00, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90
        ]),
        None
    );
    // A RIP-relative load: refused.
    assert_eq!(
        relocatable(&[
            0x48, 0x8b, 0x05, 0x00, 0x00, 0x00, 0x00, 0x90, 0x90, 0x90, 0x90, 0x90
        ]),
        None
    );
    // An instruction that straddles the 12-byte boundary: refused, because the
    // trampoline would replay a truncated instruction.
    assert_eq!(
        relocatable(&[
            0x48, 0x83, 0xec, 0x38, 0x48, 0xc7, 0xc0, 0x01, 0x00, 0x00, 0x00, 0x00
        ]),
        None
    );
}
